//! Persisted daily membership. The serving reader only sees prepared work.
use crate::{
    daily_forecast::{self, Request, Response, Score},
    daily_openings::{self, Reply, SavedLine},
    daily_plan::{self, Candidate, Prerequisites, Selection},
    db::{self, Database},
    writer::Writer,
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Local, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior::Immediate, params};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    time::{Duration, Instant},
};

const BATCH: usize = 64;
const READ_BUDGET: Duration = Duration::from_secs(15);
const CAPTURE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub struct Preparing;
impl std::fmt::Display for Preparing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Preparing the Study queue. Retry Next.")
    }
}
impl std::error::Error for Preparing {}

fn day_at(now: i64) -> Result<String> {
    Ok(DateTime::from_timestamp(now, 0)
        .context("Invalid Study clock")?
        .with_timezone(&Local)
        .date_naive()
        .to_string())
}

#[derive(Clone)]
struct Day {
    key: String,
    cutoff: i64,
    high_water: i64,
    opening_limit: usize,
    tactic_limit: usize,
    mode: String,
    due_head: Option<i64>,
    due_coverage: Option<i64>,
    openings: Option<i64>,
    tactics: Option<i64>,
    source: String,
    fresh: bool,
    forecast_finished: bool,
    order_staged: usize,
    commitments: i64,
}

fn day(db: &Connection, key: &str) -> Result<Option<Day>> {
    Ok(db.query_row(
        "SELECT cutoff,high_water,opening_limit,tactic_limit,mode,due_head,due_coverage,
                openings_version,tactics_version,opening_order,fresh_order,commitments,forecast_finished,order_staged
         FROM daily_day WHERE day=?1",
        [key],
        |r| Ok(Day {
            key: key.to_owned(), cutoff: r.get(0)?, high_water: r.get(1)?,
            opening_limit: r.get(2)?, tactic_limit: r.get(3)?, mode: r.get(4)?,
            due_head: r.get(5)?, due_coverage: r.get(6)?, openings: r.get(7)?,
            tactics: r.get(8)?, source: r.get(9)?, fresh: r.get(10)?, commitments: r.get(11)?, forecast_finished: r.get(12)?, order_staged: r.get(13)?,
        }),
    ).optional()?)
}

struct Clock {
    openings: i64,
    tactics: i64,
    coverage: i64,
    last_order: Option<String>,
    current_day: Option<String>,
}
fn clock(db: &Connection) -> Result<Clock> {
    Ok(db.query_row(
        "SELECT openings,tactics,coverage,last_order_day,current_day FROM daily_clock",
        [],
        |r| {
            Ok(Clock {
                openings: r.get(0)?,
                tactics: r.get(1)?,
                coverage: r.get(2)?,
                last_order: r.get(3)?,
                current_day: r.get(4)?,
            })
        },
    )?)
}

fn open_day(db: &mut Connection, key: &str, now: i64) -> Result<()> {
    let tx = db.transaction_with_behavior(Immediate)?;
    claim_day(&tx, key, now)?;
    tx.commit()?;
    Ok(())
}

fn claim_day(tx: &Connection, key: &str, now: i64) -> Result<()> {
    let added = tx.execute(
        "INSERT OR IGNORE INTO daily_day(day,cutoff,high_water,opening_limit,tactic_limit,mode)
         SELECT ?1,?2,c.sequence,CASE WHEN c.cutover_day=?1 THEN 0 ELSE s.opening_limit END,
           CASE WHEN c.cutover_day=?1 THEN 0 ELSE s.tactic_limit END,s.ordering
         FROM daily_clock c CROSS JOIN new_opening_settings s",
        (key, now),
    )?;
    if added != 0 {
        // An unissued new card retained in the committed deck has first claim on the new day.
        tx.execute(
            "INSERT INTO daily_new
          SELECT ?1,p.card_id,p.kind,d.slot,p.availability_version,'[]',0,1
          FROM deck d CROSS JOIN prepared_queue p ON p.card_id=d.card_id
          WHERE p.active=1 AND p.due_at IS NULL ORDER BY d.slot LIMIT 21",
            [key],
        )?;
    }
    tx.execute(
        "UPDATE daily_clock SET current_day=?1 WHERE current_day IS NOT ?1",
        [key],
    )?;
    Ok(())
}

fn taking_new(db: &Connection) -> Result<bool> {
    Ok(
        db.query_row("SELECT NOT paused FROM new_opening_settings", [], |r| {
            r.get(0)
        })?,
    )
}

/// Interactive selection probes only the head of each bounded, indexed lane.
pub fn select(
    db: &Connection,
    previous: Option<&str>,
    now: i64,
) -> Result<Option<(String, String)>> {
    let day = day(db, &day_at(now)?)?.ok_or(Preparing)?;
    let generation = day.due_head.ok_or(Preparing)?;
    let kinds = if previous == Some("opening") {
        ["tactic", "opening"]
    } else {
        ["opening", "tactic"]
    };
    for kind in kinds {
        let due = db.prepare_cached(
            "SELECT d.card_id FROM daily_due d CROSS JOIN prepared_queue p ON p.card_id=d.card_id
             WHERE d.day=?1 AND d.generation=?2 AND d.kind=?3 AND p.active=1 AND p.reserved=0
               AND p.schedule_version=d.schedule_version ORDER BY d.ordinal LIMIT 1",
        )?.query_row((&day.key, generation, kind), |r| r.get(0)).optional()?;
        if let Some(id) = due {
            return Ok(Some((id, kind.to_owned())));
        }
    }
    let clock = clock(db)?;
    let openings_enabled = day.opening_limit != 0 && taking_new(db)?;
    let opening_ready = day.openings == Some(clock.openings);
    let tactic_ready = day.tactics == Some(clock.tactics);
    for kind in kinds {
        let available = match kind {
            "opening" => openings_enabled && opening_ready,
            _ => day.tactic_limit != 0 && tactic_ready,
        };
        if !available {
            continue;
        }
        let new = db.prepare_cached(
            "SELECT n.card_id FROM daily_new n CROSS JOIN prepared_queue p ON p.card_id=n.card_id
             WHERE n.day=?1 AND n.kind=?2 AND n.committed=0 AND n.waiting=0 AND p.active=1
               AND p.reserved=0 AND p.due_at IS NULL AND p.availability_version=n.availability_version
             ORDER BY n.ordinal,n.card_id LIMIT 1",
        )?.query_row((&day.key, kind), |r| r.get(0)).optional()?;
        if let Some(id) = new {
            return Ok(Some((id, kind.to_owned())));
        }
    }
    ensure!(
        (!openings_enabled || opening_ready) && (day.tactic_limit == 0 || tactic_ready),
        Preparing
    );
    Ok(None)
}

pub fn commit_slot(db: &Connection, card: &str, now: i64) -> Result<()> {
    let day = day_at(now)?;
    if !db.query_row(
        "SELECT EXISTS(SELECT 1 FROM daily_day WHERE day=?1)",
        [&day],
        |r| r.get::<_, bool>(0),
    )? {
        claim_day(db, &day, now)?;
    }
    db.execute(
        "DELETE FROM daily_due WHERE card_id=?1 AND day=?2
         AND generation IN ((SELECT due_generation FROM daily_clock),
           (SELECT due_head FROM daily_day WHERE day=?2))",
        (card, &day),
    )?;
    if db.execute(
        "UPDATE daily_new SET committed=1 WHERE card_id=?1 AND day=?2 AND committed=0",
        (card, &day),
    )? != 0
    {
        db.execute(
            "UPDATE daily_day SET commitments=commitments+1 WHERE day=?1",
            [&day],
        )?;
    }
    Ok(())
}

/// A new accepted opening result can only change today's selected dependency rows.
pub fn refresh_parent(db: &Connection, card: &str, now: i64) -> Result<()> {
    let opening: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM prepared_queue WHERE card_id=?1 AND kind='opening')",
        [card],
        |r| r.get(0),
    )?;
    if opening {
        let day = day_at(now)?;
        db.execute(
            "UPDATE daily_new SET waiting=waiting-1
          WHERE day=?1 AND kind='opening' AND committed=0 AND waiting>0
            AND EXISTS(SELECT 1 FROM json_each(parents) WHERE value=?2)",
            (&day, card),
        )?;
        db.execute(
            "UPDATE daily_day SET commitments=commitments+1 WHERE day=?1",
            [&day],
        )?;
    }
    Ok(())
}

pub fn ready_for_repeat(db: &Connection, now: i64) -> Result<bool> {
    let Some(day) = day(db, &day_at(now)?)? else {
        return Ok(false);
    };
    let clock = clock(db)?;
    Ok(day.due_head.is_some()
        && (day.opening_limit == 0 || day.openings == Some(clock.openings) || !taking_new(db)?)
        && (day.tactic_limit == 0 || day.tactics == Some(clock.tactics)))
}

pub fn counts(db: &Connection, now: i64, unanswered: Option<&str>) -> Result<Option<Value>> {
    let Some(day) = day(db, &day_at(now)?)? else {
        return Ok(None);
    };
    let clock = clock(db)?;
    let openings = day.openings == Some(clock.openings) && taking_new(db)?;
    let tactics = day.tactics == Some(clock.tactics);
    let mut new: u32 = db.query_row(
        "SELECT count(*) FROM daily_new n
        CROSS JOIN prepared_queue p ON p.card_id=n.card_id
        WHERE n.day=?1 AND n.committed=0 AND p.active=1 AND p.reserved=0
          AND p.due_at IS NULL AND p.availability_version=n.availability_version
          AND ((n.kind='opening' AND ?2) OR (n.kind='tactic' AND ?3))",
        (&day.key, openings, tactics),
        |r| r.get(0),
    )?;
    let mut reviews: u32 = db.query_row(
        "SELECT count(*) FROM (
        SELECT 1 FROM daily_due WHERE day=?1 AND generation=?2 LIMIT 10001)",
        (&day.key, day.due_head),
        |r| r.get(0),
    )?;
    let mut committed = db.prepare_cached(
        "WITH committed(card_id) AS (
        SELECT card_id FROM deck UNION SELECT ?1 WHERE ?1 IS NOT NULL UNION
        SELECT card_id FROM (SELECT card_id FROM repeat_order ORDER BY seq LIMIT 10001))
        SELECT p.due_at IS NULL FROM committed c CROSS JOIN prepared_queue p ON p.card_id=c.card_id
        WHERE p.active=1",
    )?;
    for unseen in committed.query_map([unanswered], |r| r.get::<_, bool>(0))? {
        if unseen? {
            new += 1;
        } else {
            reviews += 1;
        }
    }
    let cap = crate::queue::COUNT_CAP as u32;
    let reviews = if reviews > cap {
        json!({"at_least":cap})
    } else {
        json!(reviews)
    };
    Ok(Some(json!({"day":day.key,"new":new,"reviews":reviews,
        "opening_order":day.source,"preparing":!ready_for_repeat(db,now)?})))
}

#[derive(Clone)]
struct Due {
    card: String,
    kind: String,
    schedule: i64,
    group: Option<String>,
    slot: Option<i64>,
}

fn due_rows(db: &Connection, day: &Day) -> Result<Vec<Due>> {
    let bank = crate::opening_rank::active_bank(db)?;
    let mut stmt = db.prepare(&format!("SELECT p.card_id,p.kind,p.schedule_version,p.scope_{bank},p.slot_{bank}
        FROM prepared_queue p WHERE p.active=1 AND p.reserved=0 AND p.due_at<=?1
          AND p.schedule_version<=?2 AND p.availability_version<=?2
          AND p.card_id IS NOT (SELECT a.card_id FROM study s JOIN attempts a ON a.id=s.active WHERE a.result IS NULL)
        ORDER BY p.due_at,p.card_id"))?;
    let rows = stmt.query_map((day.cutoff, day.high_water), |r| {
        Ok(Due {
            card: r.get(0)?,
            kind: r.get(1)?,
            schedule: r.get(2)?,
            group: r.get(3)?,
            slot: r.get(4)?,
        })
    })?;
    let mut items = Vec::new();
    let mut bytes = 0;
    for row in rows {
        let row = row?;
        bytes += row.card.len()
            + row.kind.len()
            + row.group.as_ref().map_or(0, String::len)
            + std::mem::size_of::<Due>();
        ensure!(
            bytes <= CAPTURE_BYTES,
            "Daily due snapshot exceeds its byte budget"
        );
        items.push(row);
    }
    // The earliest outstanding due opening activates its Coverage group. Within it,
    // the report's saved slot wins. The tactic stream keeps chronological order.
    let mut groups: HashMap<String, BTreeMap<(i64, usize), usize>> = HashMap::new();
    for (index, due) in items.iter().enumerate() {
        if let Some((group, slot)) = due.group.as_ref().zip(due.slot) {
            groups
                .entry(group.clone())
                .or_default()
                .insert((slot, index), index);
        }
    }
    let mut items: Vec<_> = items.into_iter().map(Some).collect();
    let mut ordered = Vec::with_capacity(items.len());
    for index in 0..items.len() {
        while let Some(row) = &items[index] {
            let head = row
                .group
                .as_ref()
                .and_then(|group| groups.get_mut(group))
                .and_then(BTreeMap::pop_first)
                .map_or(index, |(_, index)| index);
            ordered.push(items[head].take().context("Duplicate prepared due slot")?);
        }
    }
    Ok(ordered)
}

fn stage_due(
    db: &mut Connection,
    day: &Day,
    generation: i64,
    start: usize,
    rows: &[Due],
) -> Result<()> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let mut insert = tx.prepare_cached("INSERT INTO daily_due
        SELECT ?1,?2,p.card_id,p.kind,?3,p.schedule_version FROM prepared_queue p
        WHERE p.card_id=?4 AND p.active=1 AND p.reserved=0 AND p.schedule_version=?5
          AND p.availability_version<=?6
          AND p.card_id IS NOT (SELECT a.card_id FROM study s JOIN attempts a ON a.id=s.active WHERE a.result IS NULL)")?;
    for (offset, due) in rows.iter().enumerate() {
        insert.execute(params![
            day.key,
            generation,
            start + offset,
            due.card,
            due.schedule,
            day.high_water
        ])?;
    }
    drop(insert);
    tx.commit()?;
    Ok(())
}

fn activate_due(db: &Connection, day: &str, generation: i64, coverage: i64) -> Result<()> {
    db.execute(
        "UPDATE daily_day SET due_head=?2,due_coverage=?3 WHERE day=?1 AND
       (SELECT current_day=?1 AND coverage=?3 FROM daily_clock)",
        (day, generation, coverage),
    )?;
    Ok(())
}

fn reserve_due_generation(db: &Connection) -> Result<i64> {
    Ok(db.query_row(
        "UPDATE daily_clock SET due_generation=due_generation+1 RETURNING due_generation",
        [],
        |r| r.get(0),
    )?)
}

struct Draft {
    selected: Vec<(Selection, i64)>,
    kind: &'static str,
    version: i64,
    source: &'static str,
}

fn charged(db: &Connection, day: &Day, kind: &str) -> Result<usize> {
    Ok(db.query_row(
        "SELECT count(*) FROM daily_new WHERE day=?1 AND kind=?2 AND committed=1",
        (&day.key, kind),
        |r| r.get(0),
    )?)
}

fn tactic_draft(db: &Connection, day: &Day, version: i64) -> Result<Draft> {
    let limit = day.tactic_limit.saturating_sub(charged(db, day, "tactic")?);
    let mut query = db.prepare("SELECT p.card_id,p.availability_version FROM prepared_queue p
      WHERE p.kind='tactic' AND p.active=1 AND p.reserved=0 AND p.due_at IS NULL
        AND p.availability_version<=?1 AND p.schedule_version<=?1
        AND NOT EXISTS(SELECT 1 FROM daily_new n WHERE n.day=?2 AND n.card_id=p.card_id AND n.committed=1)
        AND p.card_id IS NOT (SELECT a.card_id FROM study s JOIN attempts a ON a.id=s.active WHERE a.result IS NULL)
      ORDER BY p.card_id LIMIT ?3")?;
    let selected = query
        .query_map(params![day.high_water, day.key, limit], |r| {
            Ok((
                Selection {
                    card: r.get(0)?,
                    waiting_on: Vec::new(),
                },
                r.get(1)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Draft {
        selected,
        kind: "tactic",
        version,
        source: "standard",
    })
}

struct Openings {
    basis: String,
    projection: daily_openings::OpeningProjection,
    keys: Vec<String>,
    available: HashMap<String, i64>,
    introduced: HashSet<String>,
    pinned: HashSet<String>,
    limit: usize,
    saved_order: HashMap<String, usize>,
}

fn openings(db: &Connection, day: &Day, clock: &Clock) -> Result<Openings> {
    let basis = format!(
        "{}:{}:{}:{}:{}",
        daily_forecast::PROFILE,
        db::dataset(db)?,
        day.key,
        day.high_water,
        clock.openings
    );
    let mut bytes = 0;
    let mut lines = Vec::new();
    let mut stmt =
        db.prepare("SELECT repertoire_id,color,route,start_at FROM authored_openings ORDER BY id")?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let route: String = r.get(2)?;
        bytes += route.len() + r.get_ref(0)?.as_str()?.len() + std::mem::size_of::<SavedLine>();
        ensure!(
            bytes <= CAPTURE_BYTES,
            "Daily opening snapshot exceeds its byte budget"
        );
        lines.push(SavedLine {
            repertoire: r.get(0)?,
            color: r.get(1)?,
            route: serde_json::from_str(&route)?,
            start_at: r.get(3)?,
        });
    }
    let mut replies = Vec::new();
    let mut keys = Vec::new();
    let mut available = HashMap::new();
    let mut introduced = HashSet::new();
    let mut pinned = HashSet::new();
    let mut stmt = db.prepare("SELECT c.id,c.repertoire_id,c.position_id,c.answers,p.availability_version,
            p.schedule_version,p.first_learning_version,p.due_at IS NOT NULL,p.reserved,
            coalesce(n.committed,0),
            c.id IS (SELECT a.card_id FROM study s JOIN attempts a ON a.id=s.active WHERE a.result IS NULL)
        FROM prepared_queue p CROSS JOIN cards c ON c.id=p.card_id
        LEFT JOIN daily_new n ON n.day=?1 AND n.card_id=c.id
        WHERE p.kind='opening' AND p.active=1 ORDER BY c.id")?;
    let mut rows = stmt.query([&day.key])?;
    while let Some(r) = rows.next()? {
        let card: String = r.get(0)?;
        let repertoire: String = r.get(1)?;
        let position_id: String = r.get(2)?;
        let answers: String = r.get(3)?;
        bytes += card.len()
            + repertoire.len()
            + position_id.len()
            + answers.len()
            + std::mem::size_of::<Reply>();
        ensure!(
            bytes <= CAPTURE_BYTES,
            "Daily opening snapshot exceeds its byte budget"
        );
        if let Ok([answer]) = serde_json::from_str::<[String; 1]>(&answers) {
            replies.push(Reply {
                card: card.clone(),
                repertoire,
                position_id,
                answer,
            });
        }
        let availability: i64 = r.get(4)?;
        let schedule: i64 = r.get(5)?;
        let first: Option<i64> = r.get(6)?;
        let learned: bool = r.get(7)?;
        let reserved: bool = r.get(8)?;
        let committed: bool = r.get(9)?;
        let active: bool = r.get(10)?;
        if learned {
            introduced.insert(card.clone());
        }
        if !learned && (reserved || active) {
            pinned.insert(card.clone());
        }
        if availability <= day.high_water
            && (schedule <= day.high_water && !learned
                || first.is_some_and(|first| first > day.high_water))
        {
            keys.push(card.clone());
        }
        if availability <= day.high_water
            && schedule <= day.high_water
            && !learned
            && !reserved
            && !committed
            && !active
        {
            available.insert(card, availability);
        }
    }
    let mut saved_order = HashMap::new();
    if day.mode == "maia"
        && let Some(previous) = &clock.last_order
    {
        let mut stmt =
            db.prepare("SELECT card_id,ordinal FROM daily_opening_order WHERE day=?1")?;
        for row in stmt.query_map([previous], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, usize>(1)?))
        })? {
            let (key, order) = row?;
            saved_order.insert(key, order);
        }
    }
    let projection = daily_openings::compile_openings(&lines, &replies)?;
    let limit = day
        .opening_limit
        .saturating_sub(charged(db, day, "opening")?);
    Ok(Openings {
        basis,
        projection,
        keys,
        available,
        introduced,
        pinned,
        limit,
        saved_order,
    })
}

impl Openings {
    fn ordered(&self, scores: Option<&Response>) -> Vec<String> {
        let mut ordered = self.keys.clone();
        ordered.sort_unstable_by(|a, b| {
            let score = |key: &str| match scores.and_then(|batch| batch.scores.get(key)) {
                Some(Score::Estimated { probability }) => *probability,
                _ => -1.0,
            };
            score(b)
                .total_cmp(&score(a))
                .then_with(|| {
                    self.saved_order
                        .get(a)
                        .unwrap_or(&usize::MAX)
                        .cmp(self.saved_order.get(b).unwrap_or(&usize::MAX))
                })
                .then_with(|| a.cmp(b))
        });
        ordered
    }

    fn draft(&self, version: i64, ordered: &[String], source: &'static str) -> Draft {
        let candidates: Vec<_> = ordered
            .iter()
            .filter_map(|card| {
                self.available.get(card)?;
                Some(Candidate {
                    card: card.clone(),
                    prerequisites: self
                        .projection
                        .prerequisites
                        .get(card)
                        .cloned()
                        .unwrap_or(Prerequisites::Unknown),
                })
            })
            .collect();
        let selected = daily_plan::pick(&candidates, &self.introduced, &self.pinned, self.limit)
            .into_iter()
            .map(|selection| {
                let availability = self.available[&selection.card];
                (selection, availability)
            })
            .collect();
        Draft {
            selected,
            kind: "opening",
            version,
            source,
        }
    }

    fn request(self) -> Request {
        Request {
            basis: self.basis,
            routes: self.projection.routes,
            cards: self.keys,
        }
    }
}

/// Small lane replacement (at most 20 old + 20 new rows). A reservation or
/// accepted parent during preparation wins and the local planner tries again.
fn activate_new(
    db: &mut Connection,
    day: &Day,
    draft: &Draft,
    fresh_order: Option<usize>,
) -> Result<()> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let clock = clock(&tx)?;
    let version = if draft.kind == "opening" {
        clock.openings
    } else {
        clock.tactics
    };
    let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM daily_day d CROSS JOIN daily_clock c
        WHERE d.day=?1 AND c.current_day=d.day AND d.commitments=?2 AND (?3 IS NULL OR (d.fresh_order=0 AND d.order_staged=?3)))",
        (&day.key,day.commitments,fresh_order), |r| r.get(0))?;
    if version != draft.version || !valid {
        return Ok(());
    }
    tx.execute(
        "DELETE FROM daily_new WHERE day=?1 AND kind=?2 AND committed=0",
        (&day.key, draft.kind),
    )?;
    let mut stmt = tx.prepare_cached("INSERT INTO daily_new VALUES(?1,?2,?3,?4,?5,?6,?7,0)")?;
    for (index, (choice, availability)) in draft.selected.iter().enumerate() {
        stmt.execute(params![
            day.key,
            choice.card,
            draft.kind,
            index,
            availability,
            serde_json::to_string(&choice.waiting_on)?,
            choice.waiting_on.len()
        ])?;
    }
    drop(stmt);
    if draft.kind == "opening" {
        tx.execute(
            "UPDATE daily_day SET openings_version=?2,opening_order=?3,
          fresh_order=max(fresh_order,?4) WHERE day=?1",
            (&day.key, draft.version, draft.source, fresh_order.is_some()),
        )?;
        if fresh_order.is_some() {
            tx.execute("UPDATE daily_clock SET last_order_day=?1 WHERE last_order_day IS NULL OR last_order_day<=?1", [&day.key])?;
        }
    } else {
        tx.execute(
            "UPDATE daily_day SET tactics_version=?2 WHERE day=?1",
            (&day.key, draft.version),
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn capture(path: &Path) -> Result<Connection> {
    let db = Database::read_with_busy_timeout(path, Duration::ZERO)?;
    let deadline = Instant::now() + READ_BUDGET;
    db.progress_handler(1000, Some(move || Instant::now() >= deadline));
    Ok(db)
}

fn stage_order(db: &mut Connection, day: &str, start: usize, keys: &[String]) -> Result<()> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let mut stmt =
        tx.prepare_cached("INSERT OR IGNORE INTO daily_opening_order VALUES(?1,?2,?3)")?;
    let mut added = 0;
    for (offset, key) in keys.iter().enumerate() {
        added += stmt.execute((day, key, start + offset))?;
    }
    drop(stmt);
    tx.execute(
        "UPDATE daily_day SET order_staged=order_staged+?2 WHERE day=?1",
        (day, added),
    )?;
    tx.commit()?;
    Ok(())
}

fn prune(db: &Connection, day: &str) -> Result<()> {
    // Keep the current day and a day of rollover work, plus the last complete
    // rank. A subsequent turn reclaims a bounded chunk of any older rows.
    db.execute(
        "DELETE FROM daily_due WHERE rowid IN (SELECT q.rowid FROM daily_due q
       JOIN daily_day d USING(day) WHERE q.day<date(?1,'-1 day')
       OR (q.day=?1 AND d.due_head IS NOT NULL AND q.generation<>d.due_head) LIMIT 24)",
        [day],
    )?;
    db.execute("DELETE FROM daily_opening_order WHERE rowid IN (SELECT rowid FROM daily_opening_order
        WHERE day<date(?1,'-1 day') AND day IS NOT (SELECT last_order_day FROM daily_clock) LIMIT 24)", [day])?;
    db.execute(
        "DELETE FROM daily_new WHERE rowid IN (SELECT rowid FROM daily_new
       WHERE day<date(?1,'-1 day') LIMIT 24)",
        [day],
    )?;
    db.execute("DELETE FROM daily_day WHERE day IN (SELECT d.day FROM daily_day d WHERE day<date(?1,'-1 day')
       AND day IS NOT (SELECT last_order_day FROM daily_clock)
       AND NOT EXISTS(SELECT 1 FROM daily_new WHERE day=d.day)
       AND NOT EXISTS(SELECT 1 FROM daily_due WHERE day=d.day)
       AND NOT EXISTS(SELECT 1 FROM daily_opening_order WHERE day=d.day) LIMIT 24)", [day])?;
    Ok(())
}

fn turn(path: &Path, writer: &Writer, now: i64) -> Result<bool> {
    let key = day_at(now)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let read = capture(path)?;
    let tx = read.unchecked_transaction()?;
    let Some(day) = day(&tx, &key)? else {
        writer.background_until(deadline, move |db| open_day(db, &key, now))?;
        return Ok(true);
    };
    let clock = clock(&tx)?;
    if clock.current_day.as_ref() != Some(&key) {
        tx.commit()?;
        writer.background_until(deadline, move |db| open_day(db, &key, now))?;
        return Ok(true);
    }
    if day.due_coverage != Some(clock.coverage) {
        let rows = due_rows(&tx, &day)?;
        tx.commit()?;
        let generation = writer.background_until(deadline, |db| reserve_due_generation(db))?;
        for (batch, rows) in rows.chunks(BATCH).enumerate() {
            let draft = day.clone();
            let rows = rows.to_vec();
            writer.background_until(deadline, move |db| {
                stage_due(db, &draft, generation, batch * BATCH, &rows)
            })?;
        }
        let coverage = clock.coverage;
        writer.background_until(deadline, move |db| {
            activate_due(db, &key, generation, coverage)
        })?;
        return Ok(true);
    }
    if day.tactics != Some(clock.tactics) {
        let draft = tactic_draft(&tx, &day, clock.tactics)?;
        tx.commit()?;
        writer.background_until(deadline, move |db| activate_new(db, &day, &draft, None))?;
        return Ok(true);
    }
    let directory = daily_forecast::directory(path);
    if day.openings != Some(clock.openings) {
        let draft = if day.opening_limit > 0 {
            let input = openings(&tx, &day, &clock)?;
            let source = if day.fresh {
                "maia"
            } else if input.saved_order.is_empty() {
                "standard"
            } else {
                "saved"
            };
            input.draft(clock.openings, &input.ordered(None), source)
        } else {
            Draft {
                selected: Vec::new(),
                kind: "opening",
                version: clock.openings,
                source: "standard",
            }
        };
        tx.commit()?;
        writer.background_until(deadline, move |db| activate_new(db, &day, &draft, None))?;
        return Ok(true);
    }
    if day.mode == "maia" && day.opening_limit > 0 && !day.fresh && !day.forecast_finished {
        if let Some(response) = daily_forecast::read_response(&directory, &key)? {
            let input = openings(&tx, &day, &clock)?;
            let usable = response.verify(&input.basis, &input.keys).is_ok()
                && response
                    .scores
                    .values()
                    .any(|score| matches!(score, Score::Estimated { .. }));
            if usable {
                let keys = input.ordered(Some(&response));
                let draft = input.draft(clock.openings, &keys, "maia");
                tx.commit()?;
                let staged = day.order_staged.min(keys.len());
                for (batch, keys) in keys[staged..].chunks(BATCH).enumerate() {
                    let day = key.clone();
                    let keys = keys.to_vec();
                    writer.background_until(deadline, move |db| {
                        stage_order(db, &day, staged + batch * BATCH, &keys)
                    })?;
                }
                let total = keys.len();
                writer.background_until(deadline, move |db| {
                    activate_new(db, &day, &draft, Some(total))
                })?;
                return Ok(true);
            }
            tx.commit()?;
            writer.background_until(deadline, move |db| {
                db.execute(
                    "UPDATE daily_day SET forecast_finished=1 WHERE day=?1",
                    [&key],
                )?;
                Ok(())
            })?;
            return Ok(true);
        }
        if !daily_forecast::request_exists(&directory, &key)? {
            let request = openings(&tx, &day, &clock)?.request();
            tx.commit()?;
            daily_forecast::publish_request(&directory, &key, &request)?;
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn run(path: &Path, writer: Writer, shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut last_cleanup = Instant::now();
    while !*shutdown.borrow() {
        let now = Utc::now().timestamp();
        match turn(path, &writer, now) {
            Ok(true) => continue,
            Ok(false) => {
                if last_cleanup.elapsed() >= Duration::from_secs(15) {
                    if let Ok(day) = day_at(now) {
                        let _ = writer.background(move |db| prune(db, &day));
                    }
                    last_cleanup = Instant::now();
                }
            }
            Err(error) => {
                eprintln!("Daily Study queue preparation: {error:#}");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
pub fn prepare_test_day(db: &mut Connection, now: i64) -> Result<()> {
    let key = day_at(now)?;
    open_day(db, &key, now)?;
    let day = day(db, &key)?.unwrap();
    let clock = clock(db)?;
    let rows = due_rows(db, &day)?;
    let generation = reserve_due_generation(db)?;
    for (batch, rows) in rows.chunks(BATCH).enumerate() {
        stage_due(db, &day, generation, batch * BATCH, rows)?;
    }
    activate_due(db, &key, generation, clock.coverage)?;
    let tactics = tactic_draft(db, &day, clock.tactics)?;
    activate_new(db, &day, &tactics, None)?;
    let input = openings(db, &day, &clock)?;
    let draft = input.draft(clock.openings, &input.ordered(None), "standard");
    activate_new(db, &day, &draft, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        catalog, chess, queue_store, review,
        study::{AnswerAction, AnswerIntentV1, Next},
    };
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    fn tactic(db: &Connection, card: &str, due_at: Option<i64>) -> Result<()> {
        db.execute(
            "INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
            VALUES(?1,'tactic',?1,'white',?2,?1,'[\"e2e4\"]')",
            (card, chess::fen(&chess::position("")?)),
        )?;
        if let Some(due_at) = due_at {
            db.execute(
                "INSERT INTO learning VALUES(?1,1.0,1.0,?2,?2)",
                (card, due_at),
            )?;
        }
        Ok(())
    }

    #[test]
    fn due_cutoff_and_exact_revision_survive_rebuild() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Database::init(&dir.path().join("cutoff.sqlite3"))?;
        let now = 1_751_999_400;
        tactic(&db, "due", Some(now))?;
        tactic(&db, "due-later", Some(now + 60))?;
        tactic(&db, "superseded", Some(now))?;
        while !queue_store::seed_batch(&mut db)? {}
        prepare_test_day(&mut db, now)?;
        // Even identical timestamps represent a new schedule obligation.
        db.execute(
            "UPDATE learning SET last_at=last_at,due_at=due_at WHERE card_id='superseded'",
            [],
        )?;
        tactic(&db, "newly-active", Some(now - 60))?;
        prepare_test_day(&mut db, now + 120)?;
        assert_eq!(select(&db, None, now + 120)?.unwrap().0, "due");
        commit_slot(&db, "due", now + 120)?;
        assert!(select(&db, None, now + 120)?.is_none());
        prepare_test_day(&mut db, now + 86_400)?;
        assert_eq!(select(&db, None, now + 86_400)?.unwrap().0, "newly-active");
        Ok(())
    }

    #[test]
    fn next_unlocks_exact_parent_chain_on_acceptance_with_no_source_reads() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut db = Database::init(&root.path().join("chain.sqlite3"))?;
        let now = Utc::now().timestamp();
        let line = catalog::prepare_opening(&catalog::OpeningInput {
            name: "Daily chain".into(),
            color: "white".into(),
            start_at: 0,
            route: chess::Route {
                fen: chess::fen(&chess::position("")?),
                moves: vec!["e2e4".into(), "e7e5".into(), "g1f3".into()],
            },
        })?;
        catalog::apply_opening(&db, &line, None, false)?;
        while !queue_store::seed_batch(&mut db)? {}
        prepare_test_day(&mut db, now)?;
        let initial: Vec<i64> = db
            .prepare("SELECT waiting FROM daily_new ORDER BY ordinal")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        assert_eq!(initial, vec![0, 1]);
        db.execute("UPDATE openings SET name='Display only'", [])?;
        db.flush_prepared_statement_cache();
        db.authorizer(Some(|context: AuthContext<'_>| match context.action {
            AuthAction::Read {
                table_name: "openings" | "coverage_report" | "games" | "daily_opening_order",
                ..
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }));
        let parent = review::next(
            &mut db,
            Next {
                after: None,
                dataset: None,
            },
        )?
        .unwrap();
        assert_eq!(parent.fen, chess::fen(&chess::position("")?));
        let waiting: i64 =
            db.query_row("SELECT waiting FROM daily_new WHERE committed=0", [], |r| {
                r.get(0)
            })?;
        assert_eq!(waiting, 1, "issuing the parent cannot authorize the child");
        let input = || AnswerIntentV1 {
            attempt: parent.attempt,
            dataset: parent.dataset.clone(),
            action: AnswerAction::Play("e2e4".into()),
        };
        review::answer(&mut db, input())?;
        review::answer(&mut db, input())?;
        let child = review::next(
            &mut db,
            Next {
                after: Some(parent.attempt),
                dataset: Some(parent.dataset),
            },
        )?
        .unwrap();
        assert_ne!(parent.fen, child.fen);
        let learned: i64 = db.query_row("SELECT count(*) FROM learning", [], |r| r.get(0))?;
        assert_eq!(learned, 1);
        Ok(())
    }

    #[test]
    fn forecast_flip_preserves_issued_budget_and_follows_saved_order_next_day() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("forecast.sqlite3");
        let mut db = Database::init(&path)?;
        db.execute("UPDATE new_opening_settings SET opening_limit=1", [])?;
        let fen = chess::fen(&chess::position("")?);
        for (name, movement) in [("first", "e2e4"), ("second", "d2d4")] {
            let opening = catalog::prepare_opening(&catalog::OpeningInput {
                name: name.into(),
                color: "white".into(),
                start_at: 0,
                route: chess::Route {
                    fen: fen.clone(),
                    moves: vec![movement.into()],
                },
            })?;
            catalog::apply_opening(&db, &opening, None, false)?;
        }
        while !queue_store::seed_batch(&mut db)? {}
        let now = 1_751_999_400;
        prepare_test_day(&mut db, now)?;
        let key = day_at(now)?;
        let original = day(&db, &key)?.unwrap();
        let clock_snapshot = clock(&db)?;
        let input = openings(&db, &original, &clock_snapshot)?;
        let first = select(&db, None, now)?.unwrap().0;
        let other = input.keys.iter().find(|id| **id != first).unwrap().clone();
        let batch = Response {
            basis: input.basis.clone(),
            succeeded: true,
            scores: [
                (first.clone(), Score::Estimated { probability: 0.3 }),
                (other.clone(), Score::Estimated { probability: 0.7 }),
            ]
            .into(),
        };
        let ordered = input.ordered(Some(&batch));
        let ranked = input.draft(clock_snapshot.openings, &ordered, "maia");
        stage_order(&mut db, &key, 0, &ordered)?;
        commit_slot(&db, &first, now)?;
        // A result prepared before the issued slot cannot replace it.
        activate_new(&mut db, &original, &ranked, Some(2))?;
        assert!(clock(&db)?.last_order.is_none());
        let latest = day(&db, &key)?.unwrap();
        let input = openings(&db, &latest, &clock_snapshot)?;
        let ranked = input.draft(clock_snapshot.openings, &ordered, "maia");
        activate_new(&mut db, &latest, &ranked, Some(2))?;
        assert!(select(&db, None, now)?.is_none());
        let next_day = now + 86_400;
        prepare_test_day(&mut db, next_day)?;
        assert_eq!(select(&db, None, next_day)?.unwrap().0, other);
        assert_eq!(clock(&db)?.last_order, Some(key));
        Ok(())
    }
}
