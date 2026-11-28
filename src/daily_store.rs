//! Persisted daily membership. The serving reader only sees prepared work.

pub(crate) mod forecast;
use crate::{
    daily_forecast::{self, Request, Response, Score},
    daily_openings::{self, ForecastReply, Projection, Reply, SavedLine},
    daily_plan::{self, Candidate, Group, Prerequisites, Selection},
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
    commitments: i64,
    mix_balance: i64,
}

fn day(db: &Connection, key: &str) -> Result<Option<Day>> {
    Ok(db
        .query_row(
            "SELECT cutoff,high_water,opening_limit,tactic_limit,mode,due_head,due_coverage,
                openings_version,tactics_version,opening_order,fresh_order,commitments,mix_balance
         FROM daily_day WHERE day=?1",
            [key],
            |r| {
                Ok(Day {
                    key: key.to_owned(),
                    cutoff: r.get(0)?,
                    high_water: r.get(1)?,
                    opening_limit: r.get(2)?,
                    tactic_limit: r.get(3)?,
                    mode: r.get(4)?,
                    due_head: r.get(5)?,
                    due_coverage: r.get(6)?,
                    openings: r.get(7)?,
                    tactics: r.get(8)?,
                    source: r.get(9)?,
                    fresh: r.get(10)?,
                    commitments: r.get(11)?,
                    mix_balance: r.get(12)?,
                })
            },
        )
        .optional()?)
}

struct Clock {
    openings: i64,
    authoring: i64,
    tactics: i64,
    coverage: i64,
    last_order: Option<String>,
    current_day: Option<String>,
}
fn clock(db: &Connection) -> Result<Clock> {
    Ok(db.query_row(
        "SELECT openings,tactics,coverage,last_order_day,current_day,authoring FROM daily_clock",
        [],
        |r| {
            Ok(Clock {
                openings: r.get(0)?,
                tactics: r.get(1)?,
                coverage: r.get(2)?,
                last_order: r.get(3)?,
                current_day: r.get(4)?,
                authoring: r.get(5)?,
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

struct FreshChoice {
    card: String,
    kind: String,
    balance: i64,
}

fn select(db: &Connection, day: &Day, previous: Option<&str>) -> Result<Option<FreshChoice>> {
    let kinds = if previous == Some("opening") {
        ["tactic", "opening"]
    } else {
        ["opening", "tactic"]
    };
    let clock = clock(db)?;
    let openings_enabled = day.opening_limit != 0 && taking_new(db)?;
    let opening_ready = day.openings == Some(clock.openings);
    let tactic_ready = day.tactics == Some(clock.tactics);
    let new_openings = openings_enabled && opening_ready;
    let new_tactics = day.tactic_limit != 0 && tactic_ready;

    let reviews = db
        .prepare_cached("SELECT remaining FROM daily_due_size WHERE day=?1 AND generation=?2")?
        .query_row((&day.key, day.due_head), |r| r.get(0))
        .optional()?
        .unwrap_or(0);
    let mut new_count = db.prepare_cached(
        "SELECT count(*) FROM daily_new WHERE day=?1 AND kind=?2 AND committed=0 AND waiting=0",
    )?;
    // Published membership is bounded by the two daily allowances. Lane
    // version gates cover content changes and first grades retire unseen rows.
    let openings = if new_openings {
        new_count.query_row((&day.key, "opening"), |r| r.get::<_, i64>(0))?
    } else {
        0
    };
    let tactics = if new_tactics {
        new_count.query_row((&day.key, "tactic"), |r| r.get::<_, i64>(0))?
    } else {
        0
    };
    let (preferred, balance) =
        daily_plan::mix(&day.key, day.mix_balance, reviews, openings + tactics);
    for group in [preferred, preferred.other()] {
        for kind in kinds {
            let card = match group {
                Group::Review if day.due_head.is_some() => db.prepare_cached(
                    "SELECT d.card_id FROM daily_due d CROSS JOIN prepared_queue p ON p.card_id=d.card_id
                     WHERE d.day=?1 AND d.generation=?2 AND d.kind=?3 AND p.active=1 AND p.reserved=0
                       AND p.schedule_version=d.schedule_version ORDER BY d.ordinal LIMIT 1",
                )?.query_row((&day.key, day.due_head, kind), |r| r.get(0)).optional()?,
                Group::Introduction if (kind == "opening" && new_openings)
                    || (kind == "tactic" && new_tactics) => db.prepare_cached(
                    "SELECT n.card_id FROM daily_new n CROSS JOIN prepared_queue p ON p.card_id=n.card_id
                     WHERE n.day=?1 AND n.kind=?2 AND n.committed=0 AND n.waiting=0 AND p.active=1
                       AND p.reserved=0 AND p.due_at IS NULL AND p.availability_version=n.availability_version
                     ORDER BY n.ordinal,n.card_id LIMIT 1",
                )?.query_row((&day.key, kind), |r| r.get(0)).optional()?,
                _ => None,
            };
            if let Some(card) = card {
                return Ok(Some(FreshChoice {
                    card,
                    kind: kind.to_owned(),
                    balance: if group == preferred { balance } else { 0 },
                }));
            }
        }
    }
    let openings_pending =
        !opening_ready && (openings_enabled || has_unseen_opening_commitment(db)?);
    ensure!(
        day.due_head.is_some() && !openings_pending && (day.tactic_limit == 0 || tactic_ready),
        Preparing
    );
    Ok(None)
}

/// Issue or reserve the next fresh entry inside the caller's transaction.
/// Existing committed entries use commit_slot without advancing the mix.
pub fn take_fresh(
    db: &Connection,
    previous: Option<&str>,
    now: i64,
) -> Result<Option<(String, String)>> {
    let day = day(db, &day_at(now)?)?.ok_or(Preparing)?;
    let Some(choice) = select(db, &day, previous)? else {
        return Ok(None);
    };
    commit_slot(db, &choice.card, now)?;
    if choice.balance != day.mix_balance {
        db.execute(
            "UPDATE daily_day SET mix_balance=?2 WHERE day=?1",
            (&day.key, choice.balance),
        )?;
    }
    Ok(Some((choice.card, choice.kind)))
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

/// A previously reserved, unseen opening needs the current authored-route certificate.
pub fn opening_commitments_ready(db: &Connection, now: i64) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM daily_day d CROSS JOIN daily_clock c
        WHERE d.day=?1 AND c.current_day=d.day AND d.openings_version=c.openings)",
        [day_at(now)?],
        |r| r.get(0),
    )?)
}

pub fn has_unseen_opening_commitment(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM deck d CROSS JOIN prepared_queue p ON p.card_id=d.card_id
        WHERE p.kind='opening' AND p.active=1 AND p.due_at IS NULL)",
        [],
        |r| r.get(0),
    )?)
}

fn all_lanes_ready(db: &Connection, day: &Day, clock: &Clock) -> Result<bool> {
    let openings_needed =
        day.opening_limit != 0 && taking_new(db)? || has_unseen_opening_commitment(db)?;
    Ok(day.due_head.is_some()
        && (!openings_needed || day.openings == Some(clock.openings))
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
        "opening_order":day.source,"preparing":!all_lanes_ready(db,&day,&clock)?})))
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

fn reserve_due_generation(db: &mut Connection, day: &str) -> Result<Option<i64>> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let deleted = tx.execute(
        "DELETE FROM daily_due WHERE rowid IN (
        SELECT rowid FROM daily_due WHERE day=?1
          AND generation=(SELECT due_generation FROM daily_clock)
          AND generation IS NOT (SELECT due_head FROM daily_day WHERE day=?1) LIMIT 64)",
        [day],
    )?;
    let generation = if deleted == 0 {
        Some(tx.query_row(
            "UPDATE daily_clock SET due_generation=due_generation+1 RETURNING due_generation",
            [],
            |r| r.get(0),
        )?)
    } else {
        None
    };
    tx.commit()?;
    Ok(generation)
}

struct Draft {
    selected: Vec<(Selection, i64)>,
    revoked_openings: Vec<String>,
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
    // Only explicit captures can enter after the day's availability cutoff.
    let eligible = "p.kind='tactic' AND p.active=1 AND p.reserved=0 AND p.due_at IS NULL
        AND p.schedule_version<=?1
        AND NOT EXISTS(SELECT 1 FROM daily_new n WHERE n.day=?2 AND n.card_id=p.card_id AND n.committed=1)
        AND p.card_id IS NOT (SELECT a.card_id FROM study s JOIN attempts a ON a.id=s.active WHERE a.result IS NULL)";
    let mut rows = Vec::new();
    for (index, restriction, order) in [
        (
            "prepared_capture_order",
            "p.capture_order IS NOT NULL",
            "p.capture_order,p.card_id",
        ),
        (
            "prepared_ordinary_tactic",
            "p.capture_order IS NULL AND p.availability_version<=?1",
            "p.card_id",
        ),
    ] {
        let remaining = limit - rows.len();
        if remaining == 0 {
            break;
        }
        let mut query = db.prepare(&format!(
            "SELECT p.card_id,p.availability_version FROM prepared_queue p INDEXED BY {index}
             WHERE {eligible} AND {restriction} ORDER BY {order} LIMIT ?3"
        ))?;
        for row in query.query_map(params![day.high_water, day.key, remaining], |r| {
            Ok((
                crate::catalog::daily_tactic_candidate(r.get(0)?),
                r.get::<_, i64>(1)?,
            ))
        })? {
            rows.push(row?);
        }
    }
    let (candidates, versions): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
    let selected = daily_plan::pick(&candidates, &HashSet::new(), &HashSet::new(), limit)
        .into_iter()
        .zip(versions)
        .collect();
    Ok(Draft {
        selected,
        revoked_openings: Vec::new(),
        kind: "tactic",
        version,
        source: "standard",
    })
}

struct Authored {
    basis: String,
    projection: daily_openings::OpeningProjection,
    replies: Vec<ForecastReply>,
    keys: Vec<String>,
}

impl Authored {
    fn request(self) -> Request {
        Request {
            profile: daily_forecast::PROFILE.to_owned(),
            basis: self.basis,
            routes: self.projection.routes,
            replies: self.replies,
            cards: self.keys,
        }
    }
}

struct AuthoredRows {
    basis: String,
    lines: Vec<SavedLine>,
    replies: Vec<Reply>,
    forecast_replies: Vec<ForecastReply>,
    keys: Vec<String>,
    projection: Projection,
}

impl AuthoredRows {
    fn compile(self, stop: impl Fn() -> bool) -> Result<Authored> {
        let projection =
            daily_openings::compile_openings(&self.lines, &self.replies, self.projection, stop)?;
        Ok(Authored {
            basis: self.basis,
            projection,
            replies: self.forecast_replies,
            keys: self.keys,
        })
    }
}

#[cfg(test)]
fn authored(db: &Connection, day: &Day, clock: &Clock, projection: Projection) -> Result<Authored> {
    authored_rows(db, day, clock, projection)?.compile(|| false)
}

fn authored_rows(
    db: &Connection,
    day: &Day,
    clock: &Clock,
    projection: Projection,
) -> Result<AuthoredRows> {
    let include_forecast = matches!(projection, Projection::Forecast);
    let basis = format!("{}:{}:{}", db::dataset(db)?, day.key, clock.authoring);
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
    let mut forecast_replies = Vec::new();
    let mut keys = Vec::new();
    let columns = if include_forecast {
        "color,fen"
    } else {
        "NULL,NULL"
    };
    let mut stmt = db.prepare(&format!(
        "SELECT id,repertoire_id,position_id,answers,{columns}
         FROM active_cards WHERE kind='opening' ORDER BY id"
    ))?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let card: String = r.get(0)?;
        let repertoire: String = r.get(1)?;
        let position_id: String = r.get(2)?;
        let answers: String = r.get(3)?;
        let color: Option<String> = r.get(4)?;
        let fen: Option<String> = r.get(5)?;
        bytes += color.as_ref().map_or(0, String::len)
            + fen.as_ref().map_or(0, String::len)
            + if include_forecast {
                std::mem::size_of::<ForecastReply>()
            } else {
                0
            }
            + card.len()
            + repertoire.len()
            + position_id.len()
            + answers.len()
            + std::mem::size_of::<Reply>();
        ensure!(
            bytes <= CAPTURE_BYTES,
            "Daily opening snapshot exceeds its byte budget"
        );
        if let Ok([answer]) = serde_json::from_str::<[String; 1]>(&answers) {
            if let (Some(color), Some(fen)) = (color, fen) {
                forecast_replies.push(ForecastReply {
                    repertoire: repertoire.clone(),
                    color,
                    fen,
                    movement: answer.clone(),
                    card: card.clone(),
                });
            }
            replies.push(Reply {
                card: card.clone(),
                repertoire,
                position_id,
                answer,
            });
        }
        keys.push(card);
    }
    Ok(AuthoredRows {
        basis,
        lines,
        replies,
        forecast_replies,
        keys,
        projection,
    })
}

struct Openings<T = Authored> {
    authored: T,
    available: HashMap<String, i64>,
    introduced: HashSet<String>,
    pinned: HashSet<String>,
    reserved: Vec<String>,
    limit: usize,
    saved_order: HashMap<String, usize>,
}

impl Openings<AuthoredRows> {
    fn compile(self, stop: impl Fn() -> bool) -> Result<Openings> {
        Ok(Openings {
            authored: self.authored.compile(stop)?,
            available: self.available,
            introduced: self.introduced,
            pinned: self.pinned,
            reserved: self.reserved,
            limit: self.limit,
            saved_order: self.saved_order,
        })
    }
}

fn openings(db: &Connection, day: &Day, clock: &Clock) -> Result<Openings> {
    opening_rows(db, day, clock)?.compile(|| false)
}

fn opening_rows(db: &Connection, day: &Day, clock: &Clock) -> Result<Openings<AuthoredRows>> {
    let authored = authored_rows(db, day, clock, Projection::Prerequisites)?;
    let mut available = HashMap::new();
    let mut introduced = HashSet::new();
    let mut pinned = HashSet::new();
    let mut reserved_openings = Vec::new();
    let mut stmt = db.prepare("SELECT p.card_id,p.availability_version,p.schedule_version,
            p.due_at IS NOT NULL,p.reserved,coalesce(n.committed,0),
            p.card_id IS (SELECT a.card_id FROM study s JOIN attempts a ON a.id=s.active WHERE a.result IS NULL)
        FROM prepared_queue p LEFT JOIN daily_new n ON n.day=?1 AND n.card_id=p.card_id
        WHERE p.kind='opening' AND p.active=1")?;
    let mut rows = stmt.query([&day.key])?;
    while let Some(r) = rows.next()? {
        let card: String = r.get(0)?;
        let availability: i64 = r.get(1)?;
        let schedule: i64 = r.get(2)?;
        let learned: bool = r.get(3)?;
        let reserved: bool = r.get(4)?;
        let committed: bool = r.get(5)?;
        let active: bool = r.get(6)?;
        if learned {
            introduced.insert(card.clone());
        }
        if !learned && active {
            pinned.insert(card.clone());
        }
        if !learned && reserved {
            reserved_openings.push(card.clone());
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
    let saved_order = saved_opening_order(db, day, clock)?;
    let limit = day
        .opening_limit
        .saturating_sub(charged(db, day, "opening")?);
    Ok(Openings {
        authored,
        available,
        introduced,
        pinned,
        reserved: reserved_openings,
        limit,
        saved_order,
    })
}

fn saved_opening_order(
    db: &Connection,
    day: &Day,
    clock: &Clock,
) -> Result<HashMap<String, usize>> {
    let mut saved_order = HashMap::new();
    if day.mode == "maia"
        && let Some(previous) = &clock.last_order
    {
        let mut stmt = db.prepare(
            "SELECT o.card_id,o.ordinal FROM daily_opening_order o
             JOIN daily_day d USING(day) WHERE o.day=?1 AND d.order_profile=?2",
        )?;
        for row in stmt.query_map((previous, daily_forecast::PROFILE), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, usize>(1)?))
        })? {
            let (key, order) = row?;
            saved_order.insert(key, order);
        }
    }
    Ok(saved_order)
}

fn order_openings(
    mut ordered: Vec<String>,
    saved_order: &HashMap<String, usize>,
    scores: Option<&Response>,
) -> Vec<String> {
    ordered.sort_unstable_by(|a, b| {
        let score = |key: &str| match scores.and_then(|batch| batch.scores.get(key)) {
            Some(Score::Estimated { probability }) => *probability,
            _ => -1.0,
        };
        score(b)
            .total_cmp(&score(a))
            .then_with(|| {
                saved_order
                    .get(a)
                    .unwrap_or(&usize::MAX)
                    .cmp(saved_order.get(b).unwrap_or(&usize::MAX))
            })
            .then_with(|| a.cmp(b))
    });
    ordered
}

impl Openings {
    fn ordered(&self, scores: Option<&Response>) -> Vec<String> {
        order_openings(self.authored.keys.clone(), &self.saved_order, scores)
    }

    fn draft(&self, version: i64, ordered: &[String], source: &'static str) -> Draft {
        let candidates: Vec<_> = ordered
            .iter()
            .filter_map(|card| {
                self.available.get(card)?;
                Some(Candidate {
                    card: card.clone(),
                    prerequisites: self
                        .authored
                        .projection
                        .prerequisites
                        .get(card)
                        .cloned()
                        .unwrap_or(Prerequisites::Unknown),
                })
            })
            .collect();
        let mut pinned = self.pinned.clone();
        let mut revoked_openings = Vec::new();
        for card in &self.reserved {
            let ready = match self.authored.projection.prerequisites.get(card) {
                Some(Prerequisites::NoneRequired) => true,
                Some(Prerequisites::Routes(routes)) => routes
                    .iter()
                    .any(|route| route.iter().all(|parent| self.introduced.contains(parent))),
                _ => false,
            };
            if ready {
                pinned.insert(card.clone());
            } else {
                revoked_openings.push(card.clone());
            }
        }
        let selected = daily_plan::pick(&candidates, &self.introduced, &pinned, self.limit)
            .into_iter()
            .map(|selection| {
                let availability = self.available[&selection.card];
                (selection, availability)
            })
            .collect();
        Draft {
            selected,
            revoked_openings,
            kind: "opening",
            version,
            source,
        }
    }
}

/// Small lane replacement (at most 20 old + 20 new rows). A reservation or
/// accepted parent during preparation wins and the local planner tries again.
fn activate_new(db: &mut Connection, day: &Day, draft: &Draft) -> Result<()> {
    let tx = db.transaction_with_behavior(Immediate)?;
    publish_slots(&tx, day, draft)?;
    tx.commit()?;
    Ok(())
}

fn publish_slots(tx: &Connection, day: &Day, draft: &Draft) -> Result<bool> {
    let clock = clock(tx)?;
    let version = if draft.kind == "opening" {
        clock.openings
    } else {
        clock.tactics
    };
    let valid: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM daily_day d CROSS JOIN daily_clock c
        WHERE d.day=?1 AND c.current_day=d.day AND d.commitments=?2)",
        (&day.key, day.commitments),
        |r| r.get(0),
    )?;
    if version != draft.version || !valid {
        return Ok(false);
    }
    // An authored route repair may revoke a never-issued filler. Its already
    // charged slot stays recorded for this day. Placed Again cards and issued
    // attempts use their own durable authority and are never in this list.
    for card in &draft.revoked_openings {
        tx.execute(
            "DELETE FROM deck WHERE card_id=?1 AND EXISTS(SELECT 1 FROM prepared_queue
            WHERE card_id=?1 AND kind='opening' AND due_at IS NULL)",
            [card],
        )?;
    }
    tx.execute(
        "DELETE FROM daily_new WHERE day=?1 AND kind=?2 AND committed=0",
        (&day.key, draft.kind),
    )?;
    // A tactic may have received its first grade while this draft was built.
    let mut stmt = tx.prepare_cached(
        "INSERT INTO daily_new
         SELECT ?1,?2,?3,?4,?5,?6,?7,0 FROM prepared_queue p
         WHERE p.card_id=?2 AND p.active=1 AND p.reserved=0
           AND p.due_at IS NULL AND p.availability_version=?5",
    )?;
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
            "UPDATE daily_day SET openings_version=?2,opening_order=?3 WHERE day=?1",
            (&day.key, draft.version, draft.source),
        )?;
    } else {
        tx.execute(
            "UPDATE daily_day SET tactics_version=?2 WHERE day=?1",
            (&day.key, draft.version),
        )?;
    }
    Ok(true)
}

fn capture(path: &Path) -> Result<Connection> {
    capture_before(path, Instant::now() + READ_BUDGET)
}

fn capture_before(path: &Path, limit: Instant) -> Result<Connection> {
    let deadline = limit.min(Instant::now() + READ_BUDGET);
    ensure!(Instant::now() < deadline, crate::writer::Unavailable);
    let db = Database::read_with_busy_timeout(path, Duration::ZERO)?;
    limit_capture(&db, deadline);
    Ok(db)
}

fn limit_capture(db: &Connection, deadline: Instant) {
    db.progress_handler(1000, Some(move || Instant::now() >= deadline));
}

#[cfg(test)]
fn stage_order(db: &mut Connection, day: &str, start: usize, keys: &[String]) -> Result<()> {
    let tx = db.transaction_with_behavior(Immediate)?;
    write_rank_batch(&tx, day, start, keys)?;
    tx.commit()?;
    Ok(())
}

fn write_rank_batch(tx: &Connection, day: &str, start: usize, keys: &[String]) -> Result<()> {
    let mut stmt = tx.prepare_cached("INSERT INTO daily_opening_order VALUES(?1,?2,?3)")?;
    let mut added = 0;
    for (offset, key) in keys.iter().enumerate() {
        added += stmt.execute((day, key, start + offset))?;
    }
    drop(stmt);
    tx.execute(
        "UPDATE daily_day SET order_staged=order_staged+?2 WHERE day=?1",
        (day, added),
    )?;
    Ok(())
}

fn prune(db: &Connection, day: &str) -> Result<()> {
    // Reservation clears interrupted builds before allocating a new generation.
    // Periodic cleanup can retire generations below the published head.
    db.execute(
        "DELETE FROM daily_due WHERE rowid IN (
            SELECT rowid FROM daily_due WHERE day<date(?1,'-1 day')
            UNION ALL
            SELECT rowid FROM daily_due WHERE day=?1
                AND generation<(SELECT due_head FROM daily_day WHERE day=?1)
            LIMIT 24)",
        [day],
    )?;

    let last_order_day: Option<String> = db.query_row(
        "SELECT last_order_day FROM daily_clock WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    // Exclude the saved fallback from both index ranges before walking rows.
    db.execute(
        "DELETE FROM daily_opening_order WHERE rowid IN (
            SELECT rowid FROM daily_opening_order
                WHERE day<min(date(?1,'-1 day'),coalesce(?2,date(?1,'-1 day')))
            UNION ALL
            SELECT rowid FROM daily_opening_order WHERE day>?2 AND day<date(?1,'-1 day')
            LIMIT 24)",
        (day, last_order_day),
    )?;
    db.execute(
        "DELETE FROM daily_new WHERE rowid IN (SELECT rowid FROM daily_new
       WHERE day<date(?1,'-1 day') LIMIT 24)",
        [day],
    )?;
    db.execute("DELETE FROM daily_day WHERE day IN (SELECT d.day FROM daily_day d WHERE day<date(?1,'-1 day')
       AND day IS NOT (SELECT last_order_day FROM daily_clock)
       AND NOT EXISTS(SELECT 1 FROM daily_new WHERE day=d.day)
       AND NOT EXISTS(SELECT 1 FROM daily_due WHERE day=d.day)
       AND NOT EXISTS(SELECT 1 FROM daily_opening_order WHERE day=d.day) LIMIT 16)", [day])?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum Lane {
    Day,
    Due,
    Tactics,
    Openings,
}

fn turn(path: &Path, writer: &Writer, now: i64, lane: Lane) -> Result<bool> {
    let key = day_at(now)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let read = capture(path)?;
    let tx = read.unchecked_transaction()?;
    let clock = clock(&tx)?;
    if matches!(lane, Lane::Day) {
        if clock.current_day.as_ref() == Some(&key) {
            return Ok(false);
        }
        tx.commit()?;
        writer.background_until(deadline, |db| {
            let admitted_at = Utc::now().timestamp();
            open_day(db, &day_at(admitted_at)?, admitted_at)
        })?;
        return Ok(true);
    }
    let Some(day) = day(&tx, &key)? else {
        return Ok(false);
    };
    if clock.current_day.as_ref() != Some(&key) {
        return Ok(false);
    }
    if matches!(lane, Lane::Due) && day.due_coverage != Some(clock.coverage) {
        let reserve_day = key.clone();
        let Some(generation) = writer
            .background_until(deadline, move |db| reserve_due_generation(db, &reserve_day))?
        else {
            return Ok(true);
        };
        let rows = due_rows(&tx, &day)?;
        tx.commit()?;
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
    if matches!(lane, Lane::Tactics) && day.tactics != Some(clock.tactics) {
        let draft = tactic_draft(&tx, &day, clock.tactics)?;
        tx.commit()?;
        writer.background_until(deadline, move |db| activate_new(db, &day, &draft))?;
        return Ok(true);
    }
    if matches!(lane, Lane::Openings) && day.openings != Some(clock.openings) {
        let draft = if day.opening_limit > 0 || has_unseen_opening_commitment(&tx)? {
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
                revoked_openings: Vec::new(),
                kind: "opening",
                version: clock.openings,
                source: "standard",
            }
        };
        tx.commit()?;
        writer.background_until(deadline, move |db| activate_new(db, &day, &draft))?;
        return Ok(true);
    }
    Ok(false)
}

pub fn run(path: &Path, writer: Writer, shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut last_cleanup = Instant::now();
    let mut lanes =
        [Lane::Day, Lane::Due, Lane::Tactics, Lane::Openings].map(|lane| (lane, Instant::now()));
    while !*shutdown.borrow() {
        let now = Utc::now().timestamp();
        let mut progressed = false;
        for (lane, retry) in &mut lanes {
            if *shutdown.borrow() {
                return;
            }
            if Instant::now() < *retry {
                continue;
            }
            match turn(path, &writer, now, *lane) {
                Ok(work) => progressed |= work,
                Err(error) => {
                    eprintln!("Daily Study {lane:?} preparation: {error:#}");
                    *retry = Instant::now() + Duration::from_secs(2);
                }
            }
        }
        if last_cleanup.elapsed() >= Duration::from_secs(15) {
            if let Ok(day) = day_at(now) {
                let deadline = Instant::now() + Duration::from_secs(2);
                let _ = writer.background_until(deadline, move |db| prune(db, &day));
            }
            last_cleanup = Instant::now();
        }
        if !progressed {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

#[cfg(test)]
pub fn prepare_test_day(db: &mut Connection, now: i64) -> Result<()> {
    let key = day_at(now)?;
    open_day(db, &key, now)?;
    let day = day(db, &key)?.unwrap();
    let clock = clock(db)?;
    let rows = due_rows(db, &day)?;
    let generation = loop {
        if let Some(generation) = reserve_due_generation(db, &key)? {
            break generation;
        }
    };
    for (batch, rows) in rows.chunks(BATCH).enumerate() {
        stage_due(db, &day, generation, batch * BATCH, rows)?;
    }
    activate_due(db, &key, generation, clock.coverage)?;
    let tactics = tactic_draft(db, &day, clock.tactics)?;
    activate_new(db, &day, &tactics)?;
    let input = openings(db, &day, &clock)?;
    let draft = input.draft(clock.openings, &input.ordered(None), "standard");
    activate_new(db, &day, &draft)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        catalog, chess, queue_store, review,
        study::{AnswerAction, AnswerIntentV1, Next},
    };
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    fn peek(db: &Connection, now: i64) -> Result<Option<(String, String)>> {
        let day = day(db, &day_at(now)?)?.ok_or(Preparing)?;
        Ok(select(db, &day, None)?.map(|choice| (choice.card, choice.kind)))
    }

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

    fn save_test_opening(db: &Connection, name: &str, movement: &str) -> Result<()> {
        let opening = catalog::prepare_opening(&catalog::OpeningInput {
            name: name.into(),
            color: "white".into(),
            start_at: 0,
            route: chess::Route {
                fen: chess::fen(&chess::position("")?),
                moves: vec![movement.into()],
            },
        })?;
        catalog::apply_opening(db, &opening, None, false)?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn held_forecast_socket_leaves_required_preparation_and_http_study_available()
    -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = tempfile::tempdir()?;
        let database = root.path().join("held.sqlite3");
        let mut db = Database::init(&database)?;
        let now = Utc::now().timestamp();
        tactic(&db, "review", Some(now - 1))?;
        tactic(&db, "introduction", None)?;
        save_test_opening(&db, "initial opening", "e2e4")?;
        while !queue_store::seed_batch(&mut db)? {}
        prepare_test_day(&mut db, now - 86_400)?;
        let writer = Writer::start(db, None)?;
        let (shutdown, stop) = tokio::sync::watch::channel(false);
        daily_forecast::start(database.clone(), writer.clone(), stop.clone());
        let socket = database.with_extension("dq").join("forecast.sock");
        let mut held = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(stream) = tokio::net::UnixStream::connect(&socket).await {
                    break stream;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        // An incomplete message holds the optional physical slot.
        held.write_u32(200).await?;
        held.write_all(b"{").await?;
        let planner_path = database.clone();
        let planner_writer = writer.clone();
        let planner = tokio::task::spawn_blocking(move || run(&planner_path, planner_writer, stop));
        let app = std::sync::Arc::new(crate::Runtime::new(
            database.clone(),
            writer.clone(),
            crate::uploads::Staging::start(&database)?,
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let router = crate::router(Path::new("."), port, app);
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()?;
        let base = format!("http://127.0.0.1:{port}/api/study");
        let verify = async {
            let ready = || async {
                tokio::time::timeout(Duration::from_secs(4), async {
                    loop {
                        let response = client.get(format!("{base}/counts")).send().await?;
                        if response.status().is_success() {
                            let view: Value = response.json().await?;
                            if view["day"] == day_at(now)? && view["preparing"] == false {
                                return Ok::<_, anyhow::Error>(());
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(30)).await;
                    }
                })
                .await?
            };
            ready().await?;
            writer
                .submit(false, |db| {
                    save_test_opening(db, "authored during hold", "d2d4")
                })?
                .wait()
                .await?;
            ready().await?;
            let read = Database::read(&database)?;
            let clock = clock(&read)?;
            ensure!(
                day(&read, &day_at(now)?)?.is_some_and(|day| day.openings == Some(clock.openings)),
                "Required openings did not observe the edit"
            );
            let mut next = json!({"after":null});
            let mut seen = HashSet::new();
            for _ in 0..3 {
                let card: Value = client
                    .post(format!("{base}/next"))
                    .json(&next)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                ensure!(card["attempt"].is_number(), "{card}");
                let title = card["title"]
                    .as_str()
                    .context("Missing Study title")?
                    .to_owned();
                let move_ = if title == "authored during hold" {
                    "d2d4"
                } else {
                    "e2e4"
                };
                let answer =
                    json!({"attempt":card["attempt"],"dataset":card["dataset"],"played":move_});
                let deliver = || async {
                    let reply: Value = client
                        .post(format!("{base}/answer"))
                        .json(&answer)
                        .send()
                        .await?
                        .error_for_status()?
                        .json()
                        .await?;
                    Ok::<_, anyhow::Error>(reply)
                };
                let accepted = deliver().await?;
                ensure!(accepted["rating"] == "Good", "{title} {accepted}");
                ensure!(
                    deliver().await? == accepted,
                    "Answer replay changed the result"
                );
                seen.insert(if card["kind"] == "opening" {
                    "opening".to_owned()
                } else {
                    title
                });
                next = json!({"after":card["attempt"],"dataset":card["dataset"]});
            }
            ensure!(
                seen == ["opening", "review", "introduction"]
                    .map(String::from)
                    .into(),
                "Prepared work differed: {seen:?}"
            );
            ensure!(
                tokio::time::timeout(Duration::from_millis(30), held.read_u8())
                    .await
                    .is_err(),
                "Forecast handoff did not stay held"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        drop(held);
        shutdown.send_replace(true);
        server.abort();
        planner.await?;
        verify
    }

    #[tokio::test]
    async fn forecast_ticket_publishes_and_replays_exact_evidence() -> Result<()> {
        let root = tempfile::tempdir()?;
        let database = root.path().join("ticket.sqlite3");
        let mut db = Database::init(&database)?;
        db.execute("UPDATE new_opening_settings SET opening_limit=1", [])?;
        save_test_opening(&db, "first", "e2e4")?;
        save_test_opening(&db, "second", "d2d4")?;
        while !queue_store::seed_batch(&mut db)? {}
        let now = Utc::now().timestamp();
        prepare_test_day(&mut db, now)?;
        let fallback = peek(&db, now)?.unwrap().0;
        let writer = Writer::start(db, None)?;
        tokio::task::spawn_blocking(move || {
            use crate::daily_forecast::Delivery;
            let claim = || {
                forecast::claim_today(
                    &database,
                    &writer,
                    daily_forecast::PROFILE,
                    Instant::now() + Duration::from_secs(2),
                )
            };
            let work = claim()?.expect("work");
            assert!(claim()?.is_none());
            let better = work
                .request
                .cards
                .iter()
                .find(|id| **id != fallback)
                .unwrap()
                .to_owned();
            let scores = [
                (fallback, Score::Unscored),
                (better.clone(), Score::Estimated { probability: 1.0 }),
            ]
            .into();
            let result = Response {
                profile: work.request.profile,
                basis: work.request.basis,
                scores,
                succeeded: true,
            };
            // A redelivery resumes the persisted cursor after a partial apply.
            let ticket = work.ticket.clone();
            let staged_outcome = result.clone();
            let staged_first = better.clone();
            writer.forecast(Instant::now() + Duration::from_secs(2), move |db| {
                forecast::stage_first_rank(db, &ticket, &staged_outcome, staged_first)
            })?;
            let read = Database::read(&database)?;
            assert!(clock(&read)?.last_order.is_none());
            assert_ne!(peek(&read, now)?.unwrap().0, better);
            drop(read);
            let submit =
                |result| forecast::submit(&database, &writer, work.ticket.clone(), result, now);
            assert_eq!(submit(result.clone())?, Delivery::Published);
            let read = Database::read(&database)?;
            assert_eq!(peek(&read, now)?.unwrap().0, better);
            assert_eq!(submit(result.clone())?, Delivery::Published);
            let different = Response {
                succeeded: false,
                scores: BTreeMap::new(),
                ..result
            };
            assert_eq!(submit(different)?, Delivery::Invalid);
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }

    #[test]
    fn ordinary_opening_publication_cannot_read_private_forecast_progress() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut db = Database::init(&root.path().join("ordinary-opening.sqlite3"))?;
        save_test_opening(&db, "opening", "e2e4")?;
        while !queue_store::seed_batch(&mut db)? {}
        let now = Utc::now().timestamp();
        let key = day_at(now)?;
        open_day(&mut db, &key, now)?;
        let clock = clock(&db)?;
        let day = day(&db, &key)?.unwrap();
        let input = openings(&db, &day, &clock)?;
        let draft = input.draft(clock.openings, &input.ordered(None), "standard");
        db.flush_prepared_statement_cache();
        db.authorizer(Some(|context: AuthContext<'_>| match context.action {
            AuthAction::Read {
                table_name: "forecast_claim",
                ..
            }
            | AuthAction::Read {
                table_name: "daily_day",
                column_name: "forecast_finished" | "order_staged",
                ..
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }));
        activate_new(&mut db, &day, &draft)?;
        assert_eq!(
            db.query_row("SELECT count(*) FROM daily_new", [], |row| row
                .get::<_, i64>(0))?,
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_forecast_capture_consumes_only_the_optional_daily_claim() -> Result<()> {
        let root = tempfile::tempdir()?;
        let database = root.path().join("failed-forecast.sqlite3");
        let mut db = Database::init(&database)?;
        save_test_opening(&db, "opening", "e2e4")?;
        while !queue_store::seed_batch(&mut db)? {}
        let now = Utc::now().timestamp();
        prepare_test_day(&mut db, now)?;
        let fallback = peek(&db, now)?.unwrap().0;
        // Keep the failure active across repeated claims. The prepared opening
        // has already been published and never reads this authored view.
        db.execute_batch(
            "DROP VIEW authored_openings;
             CREATE VIEW authored_openings AS
             SELECT id,repertoire_id,color,json('invalid fixture') AS route,start_at FROM openings;",
        )?;
        let writer = Writer::start(db, None)?;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let claim = || {
                forecast::claim_today(
                    &database,
                    &writer,
                    daily_forecast::PROFILE,
                    Instant::now() + Duration::from_secs(2),
                )
            };
            let failure = claim()
                .err()
                .context("Forecast unexpectedly materialized")?;
            assert!(failure.to_string().contains("malformed JSON"), "{failure}");
            assert!(
                claim()?.is_none(),
                "The failed capture must consume today's forecast"
            );
            let read = Database::read(&database)?;
            assert_eq!(
                read.query_row("SELECT outcome FROM forecast_claim", [], |row| row
                    .get::<_, String>(0))?,
                "without_rank"
            );
            assert_eq!(peek(&read, now)?.unwrap().0, fallback);
            Ok(())
        })
        .await?
    }

    const CLEANUP_DAY: &str = "2026-11-28";

    fn cleanup_fixture(db: &mut Connection, cards: usize) -> Result<()> {
        let tx = db.transaction()?;
        tx.execute(
            "WITH RECURSIVE card(n) AS (
                SELECT 1 UNION ALL SELECT n+1 FROM card WHERE n<?1
             )
             INSERT INTO cards(id,kind,title,color,fen,position_id,answers,repertoire_id)
             SELECT printf('card%05d',n),'opening','Fixture','white','fixture',printf('position%05d',n),'[]','fixture'
             FROM card",
            [cards],
        )?;
        tx.execute_batch(
            "INSERT INTO daily_day(day,cutoff,high_water,opening_limit,tactic_limit,mode,due_head)
             VALUES ('2026-11-24',0,0,20,20,'maia',NULL),
                    ('2026-11-25',0,0,20,20,'maia',NULL),
                    ('2026-11-26',0,0,20,20,'maia',NULL),
                    ('2026-11-27',0,0,20,20,'maia',1),
                    ('2026-11-28',0,0,20,20,'maia',2);
             UPDATE daily_clock SET current_day='2026-11-28', due_generation=2,
                 last_order_day='2026-11-25';",
        )?;
        tx.commit()?;
        Ok(())
    }

    fn assert_protected_cleanup_is_bounded(populate: &str) -> Result<()> {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::Relaxed},
        };

        let mut counts = Vec::new();
        for size in [100, 10_000] {
            let root = tempfile::tempdir()?;
            let mut db = Database::init(&root.path().join("prune.sqlite3"))?;
            cleanup_fixture(&mut db, size)?;
            // Keep the same day rows in both fixtures so prune has no other work.
            db.execute(
                "DELETE FROM daily_day WHERE day IN ('2026-11-24','2026-11-26')",
                [],
            )?;
            db.execute_batch(populate)?;
            let before = db.total_changes();
            let steps = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&steps);
            db.progress_handler(
                1,
                Some(move || {
                    counter.fetch_add(1, Relaxed);
                    false
                }),
            );
            let result = prune(&db, CLEANUP_DAY);
            db.progress_handler(0, None::<fn() -> bool>);
            result?;
            assert_eq!(db.total_changes(), before);
            counts.push(steps.load(Relaxed));
        }
        eprintln!("prune VM steps for 100 and 10,000 protected rows: {counts:?}");
        assert!(
            counts[1] <= 1_024 && counts[1] <= counts[0] + 64,
            "{counts:?}"
        );
        Ok(())
    }

    #[test]
    fn prune_seeks_past_published_due_rows() -> Result<()> {
        assert_protected_cleanup_is_bounded(
            "INSERT INTO daily_due SELECT '2026-11-28',2,id,kind,rowid,0 FROM cards",
        )
    }

    #[test]
    fn prune_seeks_past_saved_opening_order() -> Result<()> {
        assert_protected_cleanup_is_bounded(
            "INSERT INTO daily_opening_order SELECT '2026-11-25',id,rowid FROM cards",
        )
    }

    fn retained_cleanup_rows(db: &Connection) -> Result<Vec<String>> {
        let mut stmt = db.prepare(
            "SELECT json_array('due',day,generation,card_id,kind,ordinal,schedule_version)
               FROM daily_due WHERE day='2026-11-27' OR (day='2026-11-28' AND generation=2)
             UNION ALL
             SELECT json_array('rank',day,card_id,ordinal) FROM daily_opening_order
               WHERE day IN ('2026-11-25','2026-11-27','2026-11-28')
             UNION ALL
             SELECT json_array('day',day,due_head,due_coverage) FROM daily_day
               WHERE day IN ('2026-11-25','2026-11-27','2026-11-28')
             UNION ALL
             SELECT json_array('clock',current_day,last_order_day,due_generation) FROM daily_clock
             ORDER BY 1",
        )?;
        Ok(stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn cleanup_counts(db: &Connection) -> Result<[u64; 4]> {
        Ok(db.query_row(
            "SELECT (SELECT count(*) FROM daily_due),
                    (SELECT count(*) FROM daily_opening_order),
                    (SELECT count(*) FROM daily_new),
                    (SELECT count(*) FROM daily_day)",
            [],
            |row| Ok([row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?]),
        )?)
    }

    #[test]
    fn prune_reclaims_both_ranges_with_one_budget_per_table() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut db = Database::init(&root.path().join("mixed.sqlite3"))?;
        cleanup_fixture(&mut db, 30)?;
        db.execute_batch(
            "INSERT INTO daily_due
             SELECT d.day,1,c.id,c.kind,c.rowid,0 FROM daily_day d CROSS JOIN cards c
             WHERE d.day IN ('2026-11-24','2026-11-27','2026-11-28');
             INSERT INTO daily_due SELECT '2026-11-28',2,id,kind,rowid,0 FROM cards;
             INSERT INTO daily_opening_order
             SELECT d.day,c.id,c.rowid FROM daily_day d CROSS JOIN cards c;
             INSERT INTO daily_new
             SELECT '2026-11-26',id,kind,rowid,0,'[]',0,0 FROM cards;",
        )?;

        let retained = retained_cleanup_rows(&db)?;
        assert_eq!(cleanup_counts(&db)?, [120, 150, 30, 5]);
        for expected in [[96, 126], [72, 102], [60, 90], [60, 90]] {
            let before = cleanup_counts(&db)?;
            let writes = db.total_changes();
            prune(&db, CLEANUP_DAY)?;
            let after = cleanup_counts(&db)?;
            assert_eq!(&after[..2], &expected);
            assert!(before.into_iter().zip(after).all(|(a, b)| a - b <= 24));
            assert!(db.total_changes() - writes <= 128);
            assert_eq!(retained_cleanup_rows(&db)?, retained);
        }
        assert_eq!(cleanup_counts(&db)?, [60, 90, 0, 3]);

        db.execute("UPDATE daily_clock SET last_order_day=NULL", [])?;
        prune(&db, CLEANUP_DAY)?;
        assert_eq!(cleanup_counts(&db)?, [60, 66, 0, 3]);
        prune(&db, CLEANUP_DAY)?;
        assert_eq!(cleanup_counts(&db)?, [60, 60, 0, 2]);
        Ok(())
    }

    #[test]
    fn prune_preserves_due_builds_and_reservation_reclaims_interrupted_work() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("interrupted.sqlite3");
        let mut db = Database::init(&path)?;
        let now = 1_795_867_200;
        for index in 0..=BATCH {
            tactic(&db, &format!("due-{index}"), Some(now))?;
        }
        while !queue_store::seed_batch(&mut db)? {}
        open_day(&mut db, CLEANUP_DAY, now)?;
        let draft = day(&db, CLEANUP_DAY)?.unwrap();
        let rows = due_rows(&db, &draft)?;
        assert_eq!(rows.len(), BATCH + 1);

        let first = reserve_due_generation(&mut db, CLEANUP_DAY)?.unwrap();
        stage_due(&mut db, &draft, first, 0, &rows[..BATCH])?;
        prune(&db, CLEANUP_DAY)?;
        assert_eq!(cleanup_counts(&db)?[0], BATCH as u64);
        assert_eq!(day(&db, CLEANUP_DAY)?.unwrap().due_head, None);
        stage_due(&mut db, &draft, first, BATCH, &rows[BATCH..])?;
        let coverage = clock(&db)?.coverage;
        activate_due(&db, CLEANUP_DAY, first, coverage)?;

        // A coverage change starts another build while the first head still serves.
        db.execute("UPDATE daily_clock SET coverage=coverage+1", [])?;
        let abandoned = reserve_due_generation(&mut db, CLEANUP_DAY)?.unwrap();
        stage_due(&mut db, &draft, abandoned, 0, &rows[..BATCH])?;
        stage_due(&mut db, &draft, abandoned, BATCH, &rows[BATCH..])?;
        drop(db);

        let mut db = Database::init(&path)?;
        let generation_count = |db: &Connection, generation| -> Result<usize> {
            Ok(db.query_row(
                "SELECT count(*) FROM daily_due WHERE day=?1 AND generation=?2",
                (CLEANUP_DAY, generation),
                |row| row.get(0),
            )?)
        };
        prune(&db, CLEANUP_DAY)?;
        assert_eq!(generation_count(&db, first)?, BATCH + 1);
        assert_eq!(generation_count(&db, abandoned)?, BATCH + 1);
        assert_eq!(reserve_due_generation(&mut db, CLEANUP_DAY)?, None);
        assert_eq!(generation_count(&db, abandoned)?, 1);
        assert_eq!(reserve_due_generation(&mut db, CLEANUP_DAY)?, None);
        assert_eq!(generation_count(&db, abandoned)?, 0);

        let replacement = reserve_due_generation(&mut db, CLEANUP_DAY)?.unwrap();
        assert!(replacement > abandoned);
        stage_due(&mut db, &draft, replacement, 0, &rows[..BATCH])?;
        prune(&db, CLEANUP_DAY)?;
        assert_eq!(generation_count(&db, first)?, BATCH + 1);
        assert_eq!(generation_count(&db, replacement)?, BATCH);
        stage_due(&mut db, &draft, replacement, BATCH, &rows[BATCH..])?;
        activate_due(&db, CLEANUP_DAY, replacement, coverage + 1)?;
        assert_eq!(day(&db, CLEANUP_DAY)?.unwrap().due_head, Some(replacement));
        for _ in 0..3 {
            prune(&db, CLEANUP_DAY)?;
        }
        assert_eq!(generation_count(&db, first)?, 0);
        assert_eq!(generation_count(&db, replacement)?, BATCH + 1);
        assert_eq!(day(&db, CLEANUP_DAY)?.unwrap().due_head, Some(replacement));
        Ok(())
    }

    fn opening(db: &Connection, name: &str, moves: &[&str]) -> Result<()> {
        catalog::save_opening(
            db,
            &catalog::OpeningInput {
                name: name.to_owned(),
                color: "white".to_owned(),
                start_at: 0,
                route: chess::Route {
                    fen: chess::fen(&chess::position("")?),
                    moves: moves
                        .iter()
                        .map(|movement| (*movement).to_owned())
                        .collect(),
                },
            },
            None,
        )?;
        Ok(())
    }

    fn open_study(db: &mut Connection) -> Result<Option<crate::study::Review>> {
        review::next(
            db,
            Next {
                after: None,
                dataset: None,
            },
        )
    }

    fn answer_and_next(
        db: &mut Connection,
        issued: crate::study::Review,
        movement: &str,
    ) -> Result<Option<crate::study::Review>> {
        review::answer(
            db,
            AnswerIntentV1 {
                attempt: issued.attempt,
                dataset: issued.dataset.clone(),
                action: AnswerAction::Play(movement.to_owned()),
            },
        )?;
        review::next(
            db,
            Next {
                after: Some(issued.attempt),
                dataset: Some(issued.dataset),
            },
        )
    }

    #[test]
    fn study_next_spreads_introductions_and_restarts_with_the_same_sequence() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut db = Database::init(&root.path().join("mix.sqlite3"))?;
        let now = Utc::now().timestamp();
        for i in 0..170 {
            let due = (i < 150).then_some(now - 1);
            tactic(&db, &format!("tactic-{i:03}"), due)?;
            let name = format!("Opening {i:03}");
            opening(&db, &name, &["e2e4"])?;
            if let Some(due) = due {
                db.execute(
                    "INSERT INTO learning SELECT id,1.0,1.0,?2,?2 FROM cards WHERE title=?1",
                    (&name, due),
                )?;
            }
        }
        while !queue_store::seed_batch(&mut db)? {}
        prepare_test_day(&mut db, now)?;
        let key = day_at(now)?;
        let mut lanes = HashMap::new();
        for (intro, sql) in [
            (
                false,
                "SELECT card_id,kind FROM daily_due WHERE day=?1 AND generation=(SELECT due_head FROM daily_day WHERE day=?1) ORDER BY ordinal",
            ),
            (
                true,
                "SELECT card_id,kind FROM daily_new WHERE day=?1 AND committed=0 AND waiting=0 ORDER BY ordinal,card_id",
            ),
        ] {
            for row in db.prepare(sql)?.query_map([&key], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })? {
                let (card, kind) = row?;
                lanes
                    .entry((intro, kind))
                    .or_insert_with(std::collections::VecDeque::new)
                    .push_back(card);
            }
        }
        assert_eq!(lanes[&(true, "opening".to_owned())].len(), 20);
        assert_eq!(lanes[&(true, "tactic".to_owned())].len(), 20);
        let mut issued = open_study(&mut db)?.unwrap();
        let mut sequence = Vec::new();
        let mut positions = Vec::new();
        let mut last_kind = None;
        const RESTART_AFTER: usize = 169;
        let reopened = root.path().join("restart.sqlite3");
        for i in 0..340 {
            let card = crate::study_store::unanswered_card(&db)?.unwrap();
            let intro = db::prior_review(&db, &card)?.is_none();
            let kind = issued.kind.clone();
            if intro {
                positions.push(i);
            }
            if let Some(last) = &last_kind
                && lanes
                    .get(&(intro, kind.clone()))
                    .is_some_and(|lane| !lane.is_empty())
                && lanes.iter().any(|((is_intro, other), lane)| {
                    *is_intro == intro && *other != kind && !lane.is_empty()
                })
            {
                assert_ne!(
                    last, &kind,
                    "alternate when both kinds in the chosen group are ready"
                );
            }
            assert_eq!(
                lanes.get_mut(&(intro, kind.clone())).unwrap().pop_front(),
                Some(card.clone())
            );
            last_kind = Some(kind);
            sequence.push((issued.attempt, card));
            if i == RESTART_AFTER {
                db.backup(rusqlite::DatabaseName::Main, &reopened, None)?;
                let fresh_connection = Database::init(&reopened)?;
                fresh_connection.execute("UPDATE cards SET title='Renamed for restart'", [])?;
            }
            let next = answer_and_next(&mut db, issued, "e2e4")?;
            if i == 339 {
                assert!(next.is_none());
                break;
            }
            issued = next.unwrap();
        }
        assert!(lanes.values().all(|lane| lane.is_empty()));
        assert!(positions[0] < 20);
        for range in [0..113, 113..226, 226..340] {
            assert!(positions.iter().filter(|&&i| range.contains(&i)).count() >= 10);
        }
        let gaps: HashSet<_> = positions
            .windows(2)
            .map(|pair| pair[1] - pair[0] - 1)
            .collect();
        assert!(gaps.len() > 1 && gaps.iter().all(|gap| (1..=12).contains(gap)));
        assert_eq!(charged(&db, &day(&db, &key)?.unwrap(), "opening")?, 20);
        assert_eq!(charged(&db, &day(&db, &key)?.unwrap(), "tactic")?, 20);
        assert_eq!(
            db.query_row("SELECT count(*) FROM daily_due_size", [], |r| r
                .get::<_, i64>(0))?,
            0
        );

        let mut restarted = Database::init(&reopened)?;
        let mut next = open_study(&mut restarted)?;
        for (attempt, card) in &sequence[RESTART_AFTER..] {
            let resumed = next.unwrap();
            assert_eq!(resumed.attempt, *attempt);
            assert_eq!(
                crate::study_store::unanswered_card(&restarted)?.as_ref(),
                Some(card)
            );
            next = answer_and_next(&mut restarted, resumed, "e2e4")?;
        }
        assert!(next.is_none());
        Ok(())
    }

    #[test]
    fn captures_lead_the_next_tactic_build_within_uncommitted_allowance() -> Result<()> {
        use crate::{tactic_capture, tactic_import};
        use serde_json::json;

        let root = tempfile::tempdir()?;
        let mut db = Database::init(&root.path().join("capture-order.sqlite3"))?;
        let now = Utc::now().timestamp();
        let key = day_at(now)?;
        db.execute("UPDATE new_opening_settings SET tactic_limit=3", [])?;
        for card in ["ordinary-a", "ordinary-b", "ordinary-c"] {
            tactic(&db, card, None)?;
        }
        while !queue_store::seed_batch(&mut db)? {}
        prepare_test_day(&mut db, now)?;
        let issued = open_study(&mut db)?.unwrap();
        assert_eq!(
            crate::study_store::unanswered_card(&db)?.as_deref(),
            Some("ordinary-a")
        );
        commit_slot(&db, "ordinary-b", now)?;
        db.execute("INSERT INTO deck(slot,card_id) VALUES(1,'ordinary-b')", [])?;

        let capture = |db: &mut Connection, ordinal: i32, pgn: &str| -> Result<String> {
            let command = tactic_capture::Command::<tactic_import::Input> {
                dataset: crate::db::dataset(db)?,
                request: format!("00000000-0000-4000-8000-{ordinal:012}"),
                input: serde_json::from_value(json!({"pgn":pgn}))?,
            };
            let key = command.key("capture-pgn-v1")?;
            let plan = tactic_import::prepare(command.input)?;
            let accepted = key.apply(db, &plan)?;
            assert_eq!(key.apply(db, &plan)?, accepted);
            Ok(accepted["id"].as_str().unwrap().to_owned())
        };
        let first = capture(&mut db, 1, "1. h4 *")?;
        let second = capture(&mut db, 2, "1. a4 *")?;
        let current = day(&db, &key)?.unwrap();
        let measure = |db: &Connection| -> Result<(Draft, usize)> {
            use std::sync::{
                Arc,
                atomic::{AtomicUsize, Ordering},
            };
            let steps = Arc::new(AtomicUsize::new(0));
            let counter = steps.clone();
            let version = clock(db)?.tactics;
            assert_ne!(current.tactics, Some(version));
            db.progress_handler(
                1,
                Some(move || {
                    counter.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            );
            let draft = tactic_draft(db, &current, version);
            db.progress_handler(0, None::<fn() -> bool>);
            Ok((draft?, steps.load(Ordering::Relaxed)))
        };
        let (_, before) = measure(&db)?;
        db.execute(
            "WITH RECURSIVE rows(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM rows WHERE n<10000)
             INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
             SELECT printf('zz-unseen-%05d',n),'tactic','Ordinary','white',?1,'fixture','[\"e2e4\"]'
             FROM rows",
            [chess::fen(&chess::position("")?)],
        )?;
        let (draft, after) = measure(&db)?;
        assert!(
            after <= before + 50,
            "tactic rebuild grew from {before} to {after} VM steps"
        );
        assert_eq!(draft.selected.len(), 1);
        assert_eq!(draft.selected[0].0.card, first);
        activate_new(&mut db, &current, &draft, None)?;
        assert_eq!(open_study(&mut db)?.unwrap().attempt, issued.attempt);
        let reserved = answer_and_next(&mut db, issued, "e2e4")?.unwrap();
        assert_eq!(
            crate::study_store::unanswered_card(&db)?.as_deref(),
            Some("ordinary-b")
        );
        let captured = answer_and_next(&mut db, reserved, "e2e4")?.unwrap();
        assert_eq!(
            crate::study_store::unanswered_card(&db)?.as_deref(),
            Some(first.as_str())
        );
        assert!(answer_and_next(&mut db, captured, "h2h4")?.is_none());
        let version = clock(&db)?.tactics;
        assert_eq!(capture(&mut db, 3, "1. h4 *")?, first);
        assert_eq!(clock(&db)?.tactics, version);
        let extra = capture(&mut db, 4, "1. c4 *")?;
        let current = day(&db, &key)?.unwrap();
        let draft = tactic_draft(&db, &current, clock(&db)?.tactics)?;
        assert!(draft.selected.is_empty());
        let tomorrow = now + 86_400;
        open_day(&mut db, &day_at(tomorrow)?, tomorrow)?;
        let draft = tactic_draft(
            &db,
            &day(&db, &day_at(tomorrow)?)?.unwrap(),
            clock(&db)?.tactics,
        )?;
        let order: Vec<_> = draft
            .selected
            .iter()
            .map(|(choice, _)| choice.card.as_str())
            .collect();
        assert_eq!(order, [second.as_str(), extra.as_str(), "ordinary-c"]);
        Ok(())
    }

    #[test]
    fn pause_and_opening_prerequisites_leave_other_daily_work_available() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut db = Database::init(&root.path().join("paused.sqlite3"))?;
        let now = Utc::now().timestamp();
        opening(&db, "Opening chain", &["e2e4", "e7e5", "g1f3"])?;
        tactic(&db, "due", Some(now - 1))?;
        tactic(&db, "new", None)?;
        while !queue_store::seed_batch(&mut db)? {}
        prepare_test_day(&mut db, now)?;
        let key = day_at(now)?;
        db.execute("UPDATE new_opening_settings SET paused=1", [])?;
        let mut next = open_study(&mut db)?;
        let mut tactics = HashSet::new();
        while let Some(issued) = next {
            assert_eq!(issued.kind, "tactic");
            tactics.insert(crate::study_store::unanswered_card(&db)?.unwrap());
            next = answer_and_next(&mut db, issued, "e2e4")?;
        }
        assert_eq!(tactics, HashSet::from(["due".to_owned(), "new".to_owned()]));
        db.execute("UPDATE new_opening_settings SET paused=0", [])?;
        let parent = open_study(&mut db)?.unwrap();
        assert_eq!(parent.kind, "opening");
        assert_eq!(
            db.query_row(
                "SELECT waiting FROM daily_new WHERE day=?1 AND kind='opening' AND committed=0",
                [&key],
                |r| r.get::<_, i64>(0)
            )?,
            1
        );
        assert_eq!(open_study(&mut db)?.unwrap().attempt, parent.attempt);
        let child = answer_and_next(&mut db, parent, "e2e4")?.unwrap();
        assert_eq!(child.kind, "opening");
        assert!(answer_and_next(&mut db, child, "g1f3")?.is_none());
        Ok(())
    }

    #[test]
    fn repeat_fill_rolls_back_uncommitted_mix_progress_and_keeps_its_draw() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut db = Database::init(&root.path().join("repeat-mix.sqlite3"))?;
        let now = Utc::now().timestamp();
        tactic(&db, "due-a", Some(now - 1))?;
        tactic(&db, "due-b", Some(now - 1))?;
        tactic(&db, "new", None)?;
        tactic(&db, "repeat", Some(now + 60))?;
        while !queue_store::seed_batch(&mut db)? {}
        prepare_test_day(&mut db, now)?;
        let key = day_at(now)?;
        let issued = open_study(&mut db)?.unwrap();
        db.execute(
            "UPDATE daily_day SET openings_version=NULL WHERE day=?1",
            [&key],
        )?;
        let snapshot = |db: &Connection| -> Result<(i64, i64, i64, i64)> {
            Ok(db.query_row(
                "SELECT mix_balance,(SELECT sum(remaining) FROM daily_due_size),
                  (SELECT count(*) FROM daily_new WHERE committed=1),(SELECT count(*) FROM deck)
                 FROM daily_day WHERE day=?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?)
        };
        let before = snapshot(&db)?;
        assert_ne!(before.0, 0);
        assert_eq!(
            queue_store::place_repeat(&db, "repeat", Some("tactic"), now, 5)?,
            None
        );
        assert_eq!(snapshot(&db)?, before);
        db.execute(
            "UPDATE daily_day SET openings_version=(SELECT openings FROM daily_clock) WHERE day=?1",
            [&key],
        )?;
        assert_eq!(
            queue_store::place_repeat(&db, "repeat", Some("tactic"), now, 5)?,
            Some(2)
        );
        assert_eq!(
            db.query_row("SELECT slot FROM deck WHERE card_id='repeat'", [], |r| r
                .get::<_, i64>(0))?,
            3
        );
        drop(db);
        let mut db = Database::init(&root.path().join("repeat-mix.sqlite3"))?;
        assert_eq!(open_study(&mut db)?.unwrap().attempt, issued.attempt);
        let mut next = answer_and_next(&mut db, issued, "e2e4")?;
        for _ in 0..2 {
            let filler = next.unwrap();
            assert_ne!(filler.title, "repeat");
            next = answer_and_next(&mut db, filler, "e2e4")?;
        }
        let repeat = next.unwrap();
        assert_eq!(repeat.title, "repeat");
        assert!(answer_and_next(&mut db, repeat, "e2e4")?.is_none());
        Ok(())
    }

    #[test]
    fn first_learning_during_or_after_publication_removes_fresh_membership() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut db = Database::init(&root.path().join("first-learning.sqlite3"))?;
        let now = Utc::now().timestamp();
        let key = day_at(now)?;
        for card in ["a", "b", "c"] {
            tactic(&db, card, None)?;
        }
        while !queue_store::seed_batch(&mut db)? {}
        prepare_test_day(&mut db, now)?;
        let current_day = day(&db, &key)?.unwrap();
        let draft = tactic_draft(&db, &current_day, clock(&db)?.tactics)?;
        let remaining = |db: &Connection| -> Result<Vec<String>> {
            Ok(db
                .prepare("SELECT card_id FROM daily_new ORDER BY ordinal")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        };
        db.execute("INSERT INTO learning VALUES('a',1,1,?1,?1)", [now + 600])?;
        assert_eq!(remaining(&db)?, ["b", "c"]);
        activate_new(&mut db, &current_day, &draft)?;
        assert_eq!(remaining(&db)?, ["b", "c"]);
        db.execute("INSERT INTO learning VALUES('b',1,1,?1,?1)", [now + 600])?;
        assert_eq!(remaining(&db)?, ["c"]);
        let next = open_study(&mut db)?.unwrap();
        assert_eq!(next.title, "c");
        assert!(answer_and_next(&mut db, next, "e2e4")?.is_none());
        assert_eq!(charged(&db, &current_day, "tactic")?, 1);
        Ok(())
    }

    #[test]
    fn study_next_does_constant_sql_work_as_the_due_cohort_grows() -> Result<()> {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::Relaxed},
        };

        let root = tempfile::tempdir()?;
        let now = Utc::now().timestamp();
        let mut steps = Vec::new();
        for size in [100, 10_000] {
            let mut db = Database::init(&root.path().join(format!("work-{size}.sqlite3")))?;
            db.execute(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<?1)
                 INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
                 SELECT printf('card%05d',i),'tactic','Due','white',?2,printf('card%05d',i),'[\"e2e4\"]'
                 FROM n",
                (size, chess::fen(&chess::position("")?)),
            )?;
            db.execute(
                "INSERT INTO learning SELECT id,1,1,?1,?1 FROM cards",
                [now - 1],
            )?;
            tactic(&db, "new", None)?;
            while !queue_store::seed_batch(&mut db)? {}
            prepare_test_day(&mut db, now)?;
            let count = Arc::new(AtomicUsize::new(0));
            let measured = Arc::clone(&count);
            db.progress_handler(
                1,
                Some(move || {
                    measured.fetch_add(1, Relaxed);
                    false
                }),
            );
            let issued = open_study(&mut db);
            db.progress_handler(0, None::<fn() -> bool>);
            assert_eq!(issued?.unwrap().title, "Due");
            steps.push(count.load(Relaxed));
        }
        assert!(steps[1] <= steps[0] + 64, "{steps:?}");
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
        assert_eq!(peek(&db, now + 120)?.unwrap().0, "due");
        commit_slot(&db, "due", now + 120)?;
        assert!(peek(&db, now + 120)?.is_none());
        prepare_test_day(&mut db, now + 86_400)?;
        assert_eq!(peek(&db, now + 86_400)?.unwrap().0, "newly-active");
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
                table_name:
                    "openings" | "coverage_report" | "games" | "daily_opening_order" | "forecast_claim",
                ..
            }
            | AuthAction::Read {
                table_name: "daily_day",
                column_name: "forecast_finished" | "order_staged",
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
        let first = peek(&db, now)?.unwrap().0;
        let other = input
            .authored
            .keys
            .iter()
            .find(|id| **id != first)
            .unwrap()
            .clone();
        let batch = Response {
            profile: daily_forecast::PROFILE.to_owned(),
            basis: input.authored.basis.clone(),
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
        let tx = db.transaction_with_behavior(Immediate)?;
        forecast::publish_fresh_order(&tx, &original, &ranked, 2)?;
        tx.commit()?;
        assert!(clock(&db)?.last_order.is_none());
        let latest = day(&db, &key)?.unwrap();
        let input = openings(&db, &latest, &clock_snapshot)?;
        let ranked = input.draft(clock_snapshot.openings, &ordered, "maia");
        let tx = db.transaction_with_behavior(Immediate)?;
        forecast::publish_fresh_order(&tx, &latest, &ranked, 2)?;
        tx.commit()?;
        assert!(peek(&db, now)?.is_none());
        let next_day = now + 86_400;
        prepare_test_day(&mut db, next_day)?;
        assert_eq!(peek(&db, next_day)?.unwrap().0, other);
        assert_eq!(clock(&db)?.last_order, Some(key));
        Ok(())
    }

    #[test]
    fn forecast_request_depends_only_on_current_authored_routes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Database::init(&dir.path().join("forecast-input.sqlite3"))?;
        let input = catalog::OpeningInput {
            name: "Forecast input".into(),
            color: "white".into(),
            start_at: 0,
            route: chess::Route {
                fen: chess::fen(&chess::position("")?),
                moves: vec!["e2e4".into()],
            },
        };
        catalog::save_opening(&db, &input, None)?;
        let now = 1_751_999_400;
        let key = day_at(now)?;
        open_day(&mut db, &key, now)?;
        let today = day(&db, &key)?.unwrap();
        let prior = serde_json::to_value(
            authored(&db, &today, &clock(&db)?, Projection::Forecast)?.request(),
        )?;
        db.execute("INSERT INTO learning SELECT id,1,1,1,2 FROM cards", [])?;
        db.execute("UPDATE openings SET name='Display title'", [])?;
        let after = serde_json::to_value(
            authored(&db, &today, &clock(&db)?, Projection::Forecast)?.request(),
        )?;
        assert_eq!(prior, after);
        Ok(())
    }

    #[test]
    fn ready_tactics_and_committed_repeat_survive_unready_lanes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut db = Database::init(&root.path().join("ready.sqlite3"))?;
        let now = Utc::now().timestamp();
        tactic(&db, "repeat", Some(now))?;
        for slot in 1..=10 {
            let id = format!("card-{slot:02}");
            tactic(&db, &id, None)?;
            db.execute("INSERT INTO deck VALUES(?1,?2)", (slot, &id))?;
        }
        tactic(&db, "ready", None)?;
        catalog::save_opening(
            &db,
            &catalog::OpeningInput {
                name: "Later opening".into(),
                color: "white".into(),
                start_at: 0,
                route: chess::Route {
                    fen: chess::fen(&chess::position("")?),
                    moves: vec!["e2e4".into()],
                },
            },
            None,
        )?;
        db.execute(
            "INSERT INTO deck SELECT 11,id FROM cards WHERE kind='opening'",
            [],
        )?;
        db.execute(
            "INSERT INTO attempts(id,card_id,result) VALUES(1,'repeat','saved')",
            [],
        )?;
        queue_store::defer_repeat(&db, 1, "repeat", None, now, 5)?;
        while !queue_store::seed_batch(&mut db)? {}
        prepare_test_day(&mut db, now)?;
        db.execute(
            "UPDATE daily_day SET openings_version=NULL,due_head=NULL",
            [],
        )?;
        assert_eq!(peek(&db, now)?.unwrap().0, "ready");
        let next = review::next(
            &mut db,
            Next {
                after: None,
                dataset: None,
            },
        )?
        .unwrap();
        assert_eq!(next.title, "card-01");
        let slot: i64 =
            db.query_row("SELECT slot FROM deck WHERE card_id='repeat'", [], |row| {
                row.get(0)
            })?;
        assert_eq!(slot, 6);
        assert_eq!(
            db.query_row("SELECT count(*) FROM repeat_obligation", [], |row| row
                .get::<_, i64>(0))?,
            0
        );
        Ok(())
    }

    #[test]
    fn changed_exact_parent_revokes_an_unissued_child_reservation() -> Result<()> {
        use crate::opening_replacement;
        let dir = tempfile::tempdir()?;
        let mut db = Database::init(&dir.path().join("replaced-parent.sqlite3"))?;
        let starting = chess::position("")?;
        let line = |moves: &str| {
            catalog::prepare_opening(&catalog::OpeningInput {
                name: "Transposition".into(),
                color: "white".into(),
                start_at: 0,
                route: chess::Route {
                    fen: chess::fen(&starting),
                    moves: moves.split_whitespace().map(str::to_owned).collect(),
                },
            })
        };
        let repertoire =
            catalog::apply_opening(&db, &line("d2d4 d7d5 g1f3 g8f6 c2c4")?, None, false)?;
        while !queue_store::seed_batch(&mut db)? {}
        let now = Utc::now().timestamp();
        prepare_test_day(&mut db, now)?;
        let parent = review::next(
            &mut db,
            Next {
                after: None,
                dataset: None,
            },
        )?
        .unwrap();
        let dataset = parent.dataset;
        review::answer(
            &mut db,
            AnswerIntentV1 {
                dataset: dataset.clone(),
                attempt: parent.attempt,
                action: AnswerAction::Play("d2d4".into()),
            },
        )?;
        let reply = review::next(
            &mut db,
            Next {
                after: Some(parent.attempt),
                dataset: Some(dataset.clone()),
            },
        )?
        .unwrap();
        review::answer(
            &mut db,
            AnswerIntentV1 {
                dataset: dataset.clone(),
                attempt: reply.attempt,
                action: AnswerAction::Reveal,
            },
        )?;
        let child: String =
            db.query_row("SELECT id FROM cards WHERE answers='[\"c2c4\"]'", [], |r| {
                r.get(0)
            })?;
        let reserved = |db: &Connection| {
            db.query_row(
                "SELECT EXISTS(SELECT 1 FROM deck WHERE card_id=?1)",
                [&child],
                |r| r.get::<_, bool>(0),
            )
        };
        assert!(reserved(&db)?);
        let incoming = line("g1f3 d7d5 d2d4 g8f6 c2c4")?;
        let choices =
            opening_replacement::Choices::from([(chess::position_id(&starting), "g1f3".into())]);
        let replacement = opening_replacement::plan(
            &db,
            &repertoire,
            "white",
            &choices,
            std::slice::from_ref(&incoming),
        )?;
        let tx = db.transaction()?;
        opening_replacement::apply(&tx, &repertoire, &replacement)?;
        catalog::apply_opening(&tx, &incoming, Some(&repertoire), true)?;
        tx.commit()?;
        assert!(!opening_commitments_ready(&db, now)?);
        prepare_test_day(&mut db, now)?;
        assert!(!reserved(&db)?);
        let charged: bool = db.query_row(
            "SELECT committed FROM daily_new WHERE card_id=?1",
            [&child],
            |r| r.get(0),
        )?;
        assert!(charged);
        let next = review::next(
            &mut db,
            Next {
                after: Some(reply.attempt),
                dataset: Some(dataset),
            },
        )?;
        if let Some(next) = next {
            let issued: String = db.query_row(
                "SELECT card_id FROM attempts WHERE id=?1",
                [next.attempt],
                |r| r.get(0),
            )?;
            assert_ne!(issued, child);
        }
        Ok(())
    }

    #[test]
    fn coverage_reorders_only_frozen_due_scope() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Database::init(&dir.path().join("coverage-due.sqlite3"))?;
        let now = 1_751_999_400;
        for (name, id, schedule) in [
            ("First", "a", Some(now - 1)),
            ("Second", "b", Some(now)),
            ("Later", "c", Some(now + 60)),
            ("New", "d", None),
        ] {
            db.execute(
                "INSERT INTO cards(id,kind,title,color,fen,position_id,answers,repertoire_id)
                VALUES (?1,'opening',?2,'white',?3,?1,'[\"e2e4\"]',?1)",
                (id, name, chess::fen(&chess::position("")?)),
            )?;
            if let Some(schedule) = schedule {
                db.execute("INSERT INTO learning VALUES(?1,1,1,?2,?2)", (id, schedule))?;
            }
        }
        while !queue_store::seed_batch(&mut db)? {}
        prepare_test_day(&mut db, now)?;
        assert_eq!(peek(&db, now)?.unwrap().0, "a");
        db.execute(
            "UPDATE prepared_queue SET scope_b='applied report',
          slot_b=CASE card_id WHEN 'c' THEN 0 WHEN 'b' THEN 1 WHEN 'a' THEN 2 ELSE 3 END",
            [],
        )?;
        db.execute("UPDATE forecast_order_state SET bank=1", [])?;
        prepare_test_day(&mut db, now + 90)?;
        assert_eq!(peek(&db, now + 90)?.unwrap().0, "b");
        commit_slot(&db, "b", now + 90)?;
        assert_eq!(peek(&db, now + 90)?.unwrap().0, "a");
        let staged: i64 =
            db.query_row("SELECT count(*) FROM daily_opening_order", [], |r| r.get(0))?;
        assert_eq!(staged, 0);
        Ok(())
    }

    #[test]
    fn an_unready_opening_reservation_keeps_next_pending_while_intake_is_paused() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Database::init(&dir.path().join("pause.sqlite3"))?;
        let now = Utc::now().timestamp();
        tactic(&db, "prior", Some(now))?;
        let line = catalog::OpeningInput {
            name: "Opening".into(),
            color: "white".into(),
            start_at: 0,
            route: chess::Route {
                fen: chess::fen(&chess::position("")?),
                moves: vec!["e2e4".into()],
            },
        };
        catalog::save_opening(&db, &line, None)?;
        while !queue_store::seed_batch(&mut db)? {}
        prepare_test_day(&mut db, now)?;
        db.execute("UPDATE new_opening_settings SET paused=1", [])?;
        let previous = review::next(
            &mut db,
            Next {
                after: None,
                dataset: None,
            },
        )?
        .unwrap();
        review::answer(
            &mut db,
            AnswerIntentV1 {
                attempt: previous.attempt,
                dataset: previous.dataset.clone(),
                action: AnswerAction::Play("e2e4".into()),
            },
        )?;
        let card: String = db.query_row("SELECT id FROM cards WHERE kind='opening'", [], |r| {
            r.get(0)
        })?;
        commit_slot(&db, &card, now)?;
        db.execute("INSERT INTO deck VALUES(1,?1)", [&card])?;
        // An added authored line invalidates the certificate without retiring the reservation.
        let mut another = line;
        another.name = "Independent authored route".into();
        another.route.moves = vec!["d2d4".into()];
        catalog::save_opening(&db, &another, None)?;
        let continuation = || Next {
            after: Some(previous.attempt),
            dataset: Some(previous.dataset.clone()),
        };
        assert!(
            matches!(review::next(&mut db, continuation()), Err(error) if error.is::<Preparing>())
        );
        assert_eq!(
            crate::study_store::current(&db)?.unwrap().attempt,
            previous.attempt
        );
        assert_eq!(counts(&db, now, None)?.unwrap()["preparing"], true);
        prepare_test_day(&mut db, now)?;
        assert_eq!(
            review::next(&mut db, continuation())?.unwrap().kind,
            "opening"
        );
        Ok(())
    }
}
