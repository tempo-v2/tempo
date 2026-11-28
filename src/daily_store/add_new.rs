//! Explicit, day-scoped quota additions. The daily worker plans the request and
//! publishes the additional allowance, the ordinary lane, and its receipt together.
use super::*;
use serde::{Deserialize, Serialize};

const PENDING_LIMIT: usize = 8;
const REQUEST_LIFETIME: i64 = 60;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Opening,
    Tactic,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Opening => "opening",
            Self::Tactic => "tactic",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    request: String,
    dataset: String,
    day: String,
    kind: Kind,
    quantity: u32,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Reply {
    Pending,
    Added { added: usize },
    Expired,
    Paused,
    Unavailable,
    Missing,
    Conflict,
}

fn valid_request(request: &str) -> bool {
    request.len() == 32 && request.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn receipt(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Reply> {
    let status: Option<String> = row.get(offset)?;
    Ok(match status.as_deref() {
        Some("added") => Reply::Added {
            added: row.get(offset + 1)?,
        },
        Some("expired") => Reply::Expired,
        Some("paused") => Reply::Paused,
        Some("unavailable") => Reply::Unavailable,
        _ => Reply::Pending,
    })
}

pub fn status(db: &Connection, request: &str) -> Result<Reply> {
    ensure!(valid_request(request), "Invalid Study addition request ID");
    Ok(db
        .query_row(
            "SELECT outcome,added FROM study_add_new WHERE request=?1",
            [request],
            |row| receipt(row, 0),
        )
        .optional()?
        .unwrap_or(Reply::Missing))
}

pub fn submit(db: &mut Connection, command: Command, now: i64) -> Result<Reply> {
    ensure!(
        valid_request(&command.request),
        "Invalid Study addition request ID"
    );
    ensure!(command.quantity > 0, "Enter a positive number of cards");
    let tx = db.transaction_with_behavior(Immediate)?;
    if db::dataset(&tx)? != command.dataset {
        return Ok(Reply::Conflict);
    }
    if let Some(existing) = tx
        .query_row(
            "SELECT day,kind,quantity,outcome,added FROM study_add_new WHERE request=?1",
            [&command.request],
            |row| {
                let matches = row.get::<_, String>(0)? == command.day
                    && row.get::<_, String>(1)? == command.kind.name()
                    && row.get::<_, u32>(2)? == command.quantity;
                if matches {
                    receipt(row, 3)
                } else {
                    Ok(Reply::Conflict)
                }
            },
        )
        .optional()?
    {
        return Ok(existing);
    }
    if day_at(now)? != command.day {
        return Ok(Reply::Expired);
    }
    let pending: usize = tx.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM study_add_new WHERE day=?1 AND outcome IS NULL LIMIT ?2)",
        (&command.day, PENDING_LIMIT), |row| row.get(0),
    )?;
    ensure!(pending < PENDING_LIMIT, crate::writer::Unavailable);
    tx.execute(
        "INSERT INTO study_add_new(request,day,kind,quantity,expires) VALUES(?1,?2,?3,?4,?5)",
        (
            command.request,
            command.day,
            command.kind.name(),
            command.quantity,
            now + REQUEST_LIFETIME,
        ),
    )?;
    tx.commit()?;
    Ok(Reply::Pending)
}

struct Pending {
    request: String,
    day: String,
    kind: String,
    quantity: u32,
    expires: i64,
}

fn finish(db: &Connection, request: &str, outcome: &str, added: Option<usize>) -> Result<()> {
    ensure!(
        db.execute(
            "UPDATE study_add_new SET outcome=?2,added=?3 WHERE request=?1 AND outcome IS NULL",
            (request, outcome, added),
        )? == 1,
        "Study addition is no longer pending"
    );
    Ok(())
}

pub(super) fn turn(db: &Connection, writer: &Writer, now: i64) -> Result<bool> {
    let deadline = Instant::now() + READ_BUDGET;
    let tx = db.unchecked_transaction()?;
    let Some(request) = tx
        .query_row(
            "SELECT request,day,kind,quantity,expires FROM study_add_new
         WHERE outcome IS NULL ORDER BY day,expires,rowid LIMIT 1",
            [],
            |row| {
                Ok(Pending {
                    request: row.get(0)?,
                    day: row.get(1)?,
                    kind: row.get(2)?,
                    quantity: row.get(3)?,
                    expires: row.get(4)?,
                })
            },
        )
        .optional()?
    else {
        return Ok(false);
    };
    let today = day_at(now)?;
    if request.day != today || request.expires <= now {
        let outcome = if request.day != today {
            "expired"
        } else {
            "unavailable"
        };
        tx.commit()?;
        writer.background_until(deadline, move |db| {
            finish(db, &request.request, outcome, None)
        })?;
        return Ok(true);
    }
    let Some(day) = day(&tx, &today)? else {
        return Ok(false);
    };
    let clock = clock(&tx)?;
    if clock.current_day.as_ref() != Some(&today) {
        return Ok(false);
    }
    // Only the migration's deliberately closed introduction day overrides a
    // zero Settings allowance.
    let cutover: bool = tx.query_row(
        "SELECT cutover_day IS ?1 FROM daily_clock",
        [&today],
        |row| row.get(0),
    )?;
    let opening = request.kind == "opening";
    if cutover || opening && day.opening_paused {
        tx.commit()?;
        let outcome = if cutover { "unavailable" } else { "paused" };
        writer.background_until(deadline, move |db| {
            finish(db, &request.request, outcome, None)
        })?;
        return Ok(true);
    }

    let old_limit = if opening {
        day.opening_limit
    } else {
        day.tactic_limit
    };
    let charged = charged(&tx, &day, &request.kind)?;
    let baseline = old_limit.max(charged);
    let ceiling = baseline
        .checked_add(request.quantity as usize)
        .context("Introduction limit overflow")?;
    let mut proposed = day.clone();
    let (mut draft, overlay) = if opening {
        proposed.opening_limit = ceiling;
        let extension = priority::extended_draft(&tx, &proposed, &clock, deadline)?;
        (
            extension.draft,
            (clock.priority != 0).then_some((
                extension.prefix,
                extension.through,
                extension.basis,
                extension.updates,
            )),
        )
    } else {
        proposed.tactic_limit = ceiling;
        (tactic_draft(&tx, &proposed, clock.tactics)?, None)
    };
    let added = draft.selected.len().saturating_sub(baseline - charged);
    tx.commit()?;
    let mut prepared_overlay = None;
    if added != 0 {
        if !draft.revoked_openings.is_empty() {
            // Ordinary route maintenance owns revocation of reserved fillers.
            return Ok(false);
        }
        stage_new(Preparation::Required, writer, &day, &mut draft, deadline)?;
        if let Some((prefix, through, basis, updates)) = overlay {
            prepared_overlay = Some((
                priority::prepare_prefix(writer, &day, draft.staged, &prefix, deadline)?,
                through,
            ));
            // Keep the completion evidence durable before any of these cards
            // can be introduced, even when the next priority turn has not run.
            if !priority::save_updates(writer, &basis, &updates, deadline)? {
                return Ok(false);
            }
        }
    }

    let checked = writer.background_until(deadline, move |db| {
        let tx = db.transaction_with_behavior(Immediate)?;
        let now = Utc::now().timestamp();
        if day_at(now)? != request.day || now >= request.expires {
            return Ok(false);
        }
        if !draft_current(&tx, &day, &draft)? {
            return Ok(false);
        }
        if added != 0 {
            if !publish_slots(&tx, &day, &draft)? {
                return Ok(false);
            }
            let change = baseline - old_limit + added;
            tx.execute(
                "UPDATE daily_day SET opening_extra=opening_extra+CASE WHEN ?2 THEN ?3 ELSE 0 END,
                 tactic_extra=tactic_extra+CASE WHEN ?2 THEN 0 ELSE ?3 END WHERE day=?1",
                (&day.key, opening, change),
            )?;
            if let Some(((inline, generation), through)) = &prepared_overlay {
                let basis = crate::opening_priority::Basis::current(&tx)?
                    .context("Missing opening priority basis")?;
                crate::opening_priority::publish_prefix(
                    &tx,
                    &basis,
                    inline,
                    *generation,
                    *through,
                )?;
            }
        }
        finish(&tx, &request.request, "added", Some(added))?;
        tx.commit()?;
        Ok(true)
    })?;
    Ok(checked)
}
