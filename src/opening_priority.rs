//! Durable opening goals and a bounded resolver over compiled authored routes.
use crate::daily_plan::Prerequisites;
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet},
    time::Instant,
};

const ACTIVE: &str = "('preparing','pending','blocked')";
pub const ROOTS: usize = 64;
const MAX_CARDS: usize = 4096;
const MAX_ALTERNATIVES: usize = 8192;

pub fn join(db: &Connection, card: &str, availability: i64) -> Result<(i64, bool)> {
    if let Some(goal) = active_goal(db, card, availability)? {
        return Ok((goal, false));
    }
    let id = db.query_row(
        "INSERT INTO opening_priority(target,availability) VALUES(?1,?2) RETURNING id",
        (card, availability),
        |r| r.get(0),
    )?;
    invalidate(db)?;
    Ok((id, true))
}

fn active_goal(db: &Connection, card: &str, availability: i64) -> Result<Option<i64>> {
    Ok(db
        .query_row(
            &format!(
                "SELECT id FROM opening_priority
         WHERE target=?1 AND availability=?2 AND state IN {ACTIVE}"
            ),
            (card, availability),
            |r| r.get(0),
        )
        .optional()?)
}

fn invalidate(db: &Connection) -> Result<()> {
    db.execute(
        "UPDATE daily_clock SET priority=priority+1,openings=openings+1",
        [],
    )?;
    Ok(())
}

pub fn cancel(db: &Connection, id: i64) -> Result<()> {
    if db.execute(
        &format!(
            "UPDATE opening_priority SET state='cancelled',summary=NULL
         WHERE id=?1 AND state IN {ACTIVE}"
        ),
        [id],
    )? != 0
    {
        db.execute(
            "UPDATE daily_clock SET priority=priority+1,openings=openings+1",
            [],
        )?;
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Basis {
    pub dataset: String,
    pub day: String,
    pub openings: i64,
    pub authoring: i64,
    pub introduction_reset: i64,
    pub priority: i64,
    pub commitments: i64,
    pub paused: bool,
    #[serde(default)]
    pub allowance: usize,
}

impl Basis {
    pub fn current(db: &Connection) -> Result<Option<Self>> {
        Ok(db
            .query_row(
                "SELECT m.dataset,d.day,c.openings,c.priority,d.commitments,s.paused,c.authoring,c.introduction_reset,d.opening_limit+d.opening_extra
             FROM daily_clock c JOIN daily_day d ON d.day=c.current_day
             CROSS JOIN metadata m CROSS JOIN new_opening_settings s",
                [],
                |r| {
                    Ok(Self {
                        dataset: r.get(0)?,
                        day: r.get(1)?,
                        openings: r.get(2)?,
                        priority: r.get(3)?,
                        commitments: r.get(4)?,
                        paused: r.get(5)?,
                    allowance: r.get(8)?,
                authoring: r.get(6)?,
                introduction_reset: r.get(7)?,
                    })
                },
            )
            .optional()?)
    }
}

pub fn status(db: &Connection, id: i64) -> Result<Option<Value>> {
    let row: Option<(String, i64, String, Option<String>)> = db
        .query_row(
            "SELECT target,availability,state,summary FROM opening_priority WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((target, availability, mut state, summary)) = row else {
        return Ok(None);
    };
    let mut detail = summary
        .as_deref()
        .map(serde_json::from_str::<Value>)
        .transpose()?;
    if matches!(state.as_str(), "pending" | "blocked") {
        let current = Basis::current(db)?;
        let basis = detail
            .as_ref()
            .and_then(|v| v.get("basis"))
            .cloned()
            .map(serde_json::from_value::<Basis>)
            .transpose()?;
        let projected = state == "blocked"
            || current
                .as_ref()
                .map(|basis| published_through(db, basis))
                .transpose()?
                .flatten()
                .is_some_and(|through| through >= id);
        if current != basis || !projected {
            state = "preparing".into();
            detail = None;
        }
    }
    let introduced: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM learning WHERE card_id=?1)",
        [&target],
        |r| r.get(0),
    )?;
    Ok(Some(
        json!({"goal_id":id,"target_card_id":target,"target_availability_version":availability,
        "target_introduced":introduced,"state":state,"progress":detail}),
    ))
}

pub fn preview_status(db: &Connection, card: &str, availability: i64) -> Result<Value> {
    if let Some(goal) = active_goal(db, card, availability)? {
        return Ok(status(db, goal)?.unwrap_or(Value::Null));
    }
    let facts = db.query_row(
        "SELECT s.paused,d.opening_limit+d.opening_extra,coalesce(d.opening_charged,0)
         FROM new_opening_settings s CROSS JOIN daily_clock c
         LEFT JOIN daily_day d ON d.day=c.current_day",
        [],
        |r| {
            Ok((
                r.get::<_, bool>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    )?;
    let reason = match facts {
        (true, _, _) => Some("opening_paused"),
        (_, Some(0), _) => Some("zero_daily_allowance"),
        (_, Some(limit), charged) if limit <= charged => Some("allowance_exhausted"),
        _ => None,
    };
    Ok(json!({"state":"preparing","progress":{"reason":reason}}))
}

pub fn note_introduction(db: &Connection, card: &str) -> Result<()> {
    db.execute(
        &format!(
            "INSERT OR IGNORE INTO opening_priority_introduction
         SELECT p.card_id,c.authoring,c.introduction_reset,p.schedule_version
         FROM prepared_queue p CROSS JOIN daily_clock c
         WHERE p.card_id=?1 AND EXISTS(SELECT id FROM opening_priority WHERE state IN {ACTIVE})"
        ),
        [card],
    )?;
    Ok(())
}

#[derive(Clone)]
pub struct Goal {
    pub id: i64,
    pub target: String,
    pub availability: i64,
    pub witnessed: bool,
}

#[derive(Deserialize)]
struct Witness {
    authoring: i64,
    introduction_reset: i64,
    schedule_floor: i64,
}

#[derive(Deserialize)]
struct PendingProof {
    #[serde(default)]
    completion_basis: Option<Witness>,
    #[serde(default)]
    next: Vec<WitnessCard>,
}

#[derive(Deserialize)]
struct WitnessCard {
    card_id: String,
}

pub fn roots(db: &Connection, after: i64) -> Result<Vec<Goal>> {
    let mut query = db.prepare(&format!(
        "SELECT id,target,availability,summary FROM opening_priority
         WHERE id>?1 AND state IN {ACTIVE} ORDER BY id LIMIT ?2"
    ))?;
    let mut rows = query.query((after, ROOTS))?;
    let mut result = Vec::new();
    let mut introduced = db.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM opening_priority_introduction
         WHERE card_id=?1 AND authoring=?2 AND introduction_reset=?3 AND schedule_version>?4)",
    )?;
    while let Some(row) = rows.next()? {
        let summary: Option<String> = row.get(3)?;
        let mut witnessed = false;
        if let Some(summary) = summary {
            let proof: PendingProof = serde_json::from_str(&summary)?;
            if let Some(basis) = proof.completion_basis {
                witnessed = !proof.next.is_empty();
                for card in proof.next {
                    let found: bool = introduced.query_row(
                        (
                            card.card_id,
                            basis.authoring,
                            basis.introduction_reset,
                            basis.schedule_floor,
                        ),
                        |r| r.get(0),
                    )?;
                    if !found {
                        witnessed = false;
                        break;
                    }
                }
            }
        }
        result.push(Goal {
            id: row.get(0)?,
            target: row.get(1)?,
            availability: row.get(2)?,
            witnessed,
        });
    }
    Ok(result)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Proof {
    pub card: String,
    // Only unseen parents enter the daily certificate.
    pub parents: Vec<String>,
}

pub fn published_prefix(db: &Connection, basis: &Basis) -> Result<Option<Vec<Proof>>> {
    let prefix: Option<String> = db
        .query_row(
            "SELECT prefix FROM opening_priority_projection
         WHERE day=?1 AND openings=?2 AND priority=?3 AND commitments=?4 AND paused=?5 AND allowance=?6",
            params![
                basis.day,
                basis.openings,
                basis.priority,
                basis.commitments,
                basis.paused,
                basis.allowance,
            ],
            |r| r.get(0),
        )
        .optional()?;
    prefix.map(|p| Ok(serde_json::from_str(&p)?)).transpose()
}

pub fn published_through(db: &Connection, basis: &Basis) -> Result<Option<i64>> {
    Ok(db
        .query_row(
            "SELECT processed_through FROM opening_priority_projection
         WHERE day=?1 AND openings=?2 AND priority=?3 AND commitments=?4 AND paused=?5 AND allowance=?6",
            params![
                basis.day,
                basis.openings,
                basis.priority,
                basis.commitments,
                basis.paused,
                basis.allowance,
            ],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn publish_prefix(
    db: &Connection,
    basis: &Basis,
    prefix: &[Proof],
    through: i64,
) -> Result<()> {
    ensure!(
        prefix.len() <= basis.allowance,
        "opening priority prefix exceeds daily allowance"
    );
    db.execute(
        "INSERT OR REPLACE INTO opening_priority_projection
        (singleton,day,openings,priority,commitments,paused,prefix,processed_through,allowance) VALUES (1,?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            basis.day,
            basis.openings,
            basis.priority,
            basis.commitments,
            basis.paused,
            serde_json::to_string(prefix)?, through, basis.allowance
        ],
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Block {
    Unknown,
    Unavailable,
    Cycle,
    Limit,
}

impl Block {
    pub fn code(self) -> &'static str {
        match self {
            Self::Unknown => "unknown_prerequisites",
            Self::Unavailable => "unavailable_parent",
            Self::Cycle => "dependency_cycle",
            Self::Limit => "resolution_limit",
        }
    }
}

pub struct Graph<'a> {
    pub routes: &'a HashMap<String, Prerequisites>,
    pub availability: &'a HashMap<String, i64>,
    pub introduced: &'a HashSet<String>,
}

struct Frame {
    card: String,
    routes: BinaryHeap<Reverse<(usize, usize)>>,
    prepared_routes: usize,
    current_route: Option<usize>,
    next_parent: usize,
    checkpoint: usize,
    reason: Block,
}

impl Frame {
    fn current<'a>(&self, graph: &Graph<'a>) -> &'a [String] {
        match &graph.routes[&self.card] {
            Prerequisites::Routes(routes) => &routes[self.current_route.expect("selected route")],
            Prerequisites::NoneRequired => &[],
            Prerequisites::Unknown => unreachable!("unknown routes never enter the resolver"),
        }
    }
}

pub enum Resolution {
    More,
    Complete(Vec<Proof>),
    Blocked(Block),
}

/// Carries an unfinished DFS across bounded planning turns. A failed branch
/// discards only additions after that branch's checkpoint.
#[derive(Default)]
pub struct Resolver {
    stack: Vec<Frame>,
    on_stack: HashSet<String>,
    examined: HashSet<String>,
    resolved: HashSet<String>,
    order: Vec<Proof>,
    trials: usize,
}

impl Resolver {
    fn enter(&mut self, card: &str, graph: &Graph<'_>) -> Result<(), Block> {
        if self.on_stack.contains(card) {
            return Err(Block::Cycle);
        }
        if self.examined.insert(card.to_owned()) && self.examined.len() > MAX_CARDS {
            return Err(Block::Limit);
        }
        if !graph.availability.contains_key(card) {
            return Err(Block::Unavailable);
        }
        match graph.routes.get(card) {
            Some(Prerequisites::NoneRequired) => {}
            Some(Prerequisites::Routes(routes)) if !routes.is_empty() => {}
            _ => return Err(Block::Unknown),
        }
        self.on_stack.insert(card.to_owned());
        self.stack.push(Frame {
            card: card.to_owned(),
            routes: BinaryHeap::new(),
            prepared_routes: 0,
            current_route: None,
            next_parent: 0,
            checkpoint: self.order.len(),
            reason: Block::Unknown,
        });
        Ok(())
    }

    fn abandon(&mut self, reason: Block) {
        let frame = self.stack.last_mut().unwrap();
        frame.reason = match (frame.reason, reason) {
            (Block::Cycle, _) | (_, Block::Cycle) => Block::Cycle,
            (Block::Unavailable, _) | (_, Block::Unavailable) => Block::Unavailable,
            _ => reason,
        };
        for entry in self.order.drain(frame.checkpoint..) {
            self.resolved.remove(&entry.card);
        }
        frame.current_route = None;
        frame.next_parent = 0;
    }

    pub fn resume(&mut self, root: &str, graph: &Graph<'_>, deadline: Instant) -> Resolution {
        if self.stack.is_empty()
            && let Err(reason) = self.enter(root, graph)
        {
            return Resolution::Blocked(reason);
        }
        loop {
            if Instant::now() >= deadline {
                return Resolution::More;
            }
            let frame = self.stack.last_mut().unwrap();
            let routes = &graph.routes[&frame.card];
            // Count and enqueue one alternative per iteration so a large
            // authored frontier is itself resumable across time slices.
            if let Prerequisites::Routes(routes) = routes
                && let Some(route) = routes.get(frame.prepared_routes)
            {
                let unseen = route
                    .iter()
                    .filter(|id| !graph.introduced.contains(*id))
                    .count();
                // The compiler emits alternative vectors in canonical order.
                frame.routes.push(Reverse((unseen, frame.prepared_routes)));
                frame.prepared_routes += 1;
                continue;
            }
            if frame.current_route.is_none() {
                let route = match routes {
                    Prerequisites::NoneRequired => Some(0),
                    _ => frame.routes.pop().map(|Reverse((_, index))| index),
                };
                let Some(route) = route else {
                    let frame = self.stack.pop().unwrap();
                    self.on_stack.remove(&frame.card);
                    if self.stack.is_empty() {
                        return Resolution::Blocked(frame.reason);
                    }
                    self.abandon(frame.reason);
                    continue;
                };
                self.trials += 1;
                if self.trials > MAX_ALTERNATIVES {
                    return Resolution::Blocked(Block::Limit);
                }
                frame.current_route = Some(route);
                frame.next_parent = 0;
                frame.checkpoint = self.order.len();
            }
            let idx = frame.next_parent;
            if let Some(parent) = frame.current(graph).get(idx).cloned() {
                frame.next_parent += 1;
                if graph.introduced.contains(&parent) {
                    if self.examined.insert(parent.clone()) && self.examined.len() > MAX_CARDS {
                        return Resolution::Blocked(Block::Limit);
                    }
                    if !graph.availability.contains_key(&parent) {
                        self.abandon(Block::Unavailable);
                    }
                } else if !self.resolved.contains(&parent) {
                    match self.enter(&parent, graph) {
                        Ok(()) => {}
                        Err(Block::Limit) => return Resolution::Blocked(Block::Limit),
                        Err(reason) => self.abandon(reason),
                    }
                }
                continue;
            }
            let frame = self.stack.pop().unwrap();
            self.on_stack.remove(&frame.card);
            if !graph.introduced.contains(&frame.card) {
                let parents = frame
                    .current(graph)
                    .iter()
                    .filter(|id| !graph.introduced.contains(*id))
                    .cloned()
                    .collect();
                self.resolved.insert(frame.card.clone());
                self.order.push(Proof {
                    card: frame.card,
                    parents,
                });
            }
            if self.stack.is_empty() {
                return Resolution::Complete(std::mem::take(&mut self.order));
            }
        }
    }
}

#[derive(Clone)]
pub struct Update {
    pub id: i64,
    pub state: &'static str,
    pub summary: Value,
}

pub fn publish_updates(db: &Connection, basis: &Basis, entries: &[Update]) -> Result<bool> {
    if Basis::current(db)?.as_ref() != Some(basis) {
        return Ok(false);
    }
    let mut q = db.prepare_cached(&format!(
        "UPDATE opening_priority SET state=?2,summary=?3 WHERE id=?1 AND state IN {ACTIVE}"
    ))?;
    for entry in entries {
        let mut summary = entry.summary.clone();
        summary["basis"] = serde_json::to_value(basis)?;
        let text = serde_json::to_string(&summary)?;
        ensure!(
            text.len() <= 15 * 1024,
            "opening priority status exceeds its bound"
        );
        q.execute(params![entry.id, entry.state, text])?;
    }
    Ok(true)
}
