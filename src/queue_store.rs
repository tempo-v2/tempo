use crate::{
    daily_store::{self, Preparing},
    queue::{self, NewOpeningOrder, NewOpeningSettings, UpdateNewOpeningSettings},
};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

pub fn new_opening_settings(db: &Connection) -> Result<NewOpeningSettings> {
    let (paused, ordering, opening_limit, tactic_limit): (bool, String, u8, u8) = db.query_row(
        "SELECT paused,ordering,opening_limit,tactic_limit FROM new_opening_settings WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let ordering = match ordering.as_str() {
        "maia" => NewOpeningOrder::Maia,
        "standard" => NewOpeningOrder::Standard,
        _ => anyhow::bail!("Unknown new opening order"),
    };
    Ok(NewOpeningSettings {
        paused,
        ordering,
        opening_limit,
        tactic_limit,
    })
}

pub fn save_new_opening_settings(
    db: &Connection,
    update: UpdateNewOpeningSettings,
) -> Result<NewOpeningSettings> {
    let saved = new_opening_settings(db)?;
    let preferences = NewOpeningSettings {
        paused: update.paused,
        ordering: update.ordering,
        opening_limit: update.opening_limit.unwrap_or(saved.opening_limit),
        tactic_limit: update.tactic_limit.unwrap_or(saved.tactic_limit),
    };
    anyhow::ensure!(
        preferences.opening_limit <= queue::MAX_DAILY_NEW
            && preferences.tactic_limit <= queue::MAX_DAILY_NEW,
        "Daily new card limits must be between 0 and {}.",
        queue::MAX_DAILY_NEW
    );
    db.execute(
        "UPDATE new_opening_settings SET paused=?1,ordering=?2,opening_limit=?3,tactic_limit=?4 WHERE singleton=1",
        params![preferences.paused, preferences.ordering.as_str(), preferences.opening_limit, preferences.tactic_limit],
    )?;
    Ok(preferences)
}

pub fn can_repeat(db: &Connection, card: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM active_cards WHERE id=?1)",
        [card],
        |r| r.get(0),
    )?)
}

pub fn committed_head(db: &Connection, now: i64) -> Result<Option<(String, String)>> {
    Ok(db.query_row(
        "SELECT d.card_id,p.kind FROM deck d CROSS JOIN prepared_queue p ON p.card_id=d.card_id
          WHERE p.active=1 AND (?1 OR p.kind<>'opening' OR p.due_at IS NOT NULL) ORDER BY d.slot LIMIT 1",
        [daily_store::opening_commitments_ready(db,now)?], |r| Ok((r.get(0)?,r.get(1)?)),
    ).optional()?)
}

pub fn consume(db: &Connection, card: &str) -> Result<()> {
    db.execute("DELETE FROM deck WHERE card_id=?1", [card])?;
    Ok(())
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepeatOutcome {
    None,
    Placed { offset: u8, draw: u8 },
    Deferred { obligation: i64, draw: u8 },
    AlreadyQueued { slot: i64 },
    AlreadyPending { obligation: i64 },
}

impl RepeatOutcome {
    pub fn study(&self) -> crate::study::RepeatDisposition {
        use crate::study::RepeatDisposition;
        match self {
            Self::Placed { offset, .. } => RepeatDisposition::Placed(*offset),
            Self::Deferred { .. } | Self::AlreadyPending { .. } => RepeatDisposition::Deferred,
            Self::None | Self::AlreadyQueued { .. } => RepeatDisposition::None,
        }
    }
}

pub fn admit_repeat(
    db: &Connection,
    card: &str,
    attempt: i64,
    previous: Option<&str>,
    now: i64,
    write_budget: Option<u64>,
) -> Result<RepeatOutcome> {
    if !can_repeat(db, card)? {
        return Ok(RepeatOutcome::None);
    }
    if let Some(slot) = db
        .query_row("SELECT slot FROM deck WHERE card_id=?1", [card], |r| {
            r.get(0)
        })
        .optional()?
    {
        return Ok(RepeatOutcome::AlreadyQueued { slot });
    }
    if let Some(obligation) = db
        .query_row(
            "SELECT attempt_id FROM repeat_obligation WHERE card_id=?1",
            [card],
            |r| r.get(0),
        )
        .optional()?
    {
        return Ok(RepeatOutcome::AlreadyPending { obligation });
    }
    let draw = queue::draw_repeat_offset();
    if can_place_now(db)?
        && let Some(offset) = place_repeat_bounded(db, card, previous, now, draw, write_budget)?
    {
        return Ok(RepeatOutcome::Placed { offset, draw });
    }
    defer_repeat(db, attempt, card, previous, now, draw)?;
    Ok(RepeatOutcome::Deferred {
        obligation: attempt,
        draw,
    })
}

// A budget is an absolute SQLite total_changes ceiling for speculative placement.
// Check before each bounded write group. Rolled-back effects still count.
fn check_repeat_budget(db: &Connection, budget: Option<u64>, writes: u64) -> Result<()> {
    anyhow::ensure!(
        budget.is_none_or(|end| db.total_changes() + writes <= end),
        Preparing
    );
    Ok(())
}

/// Preserve the original draw and leave partial reservations untouched when
/// another lane is still preparing. The caller saves the repeat obligation.
pub fn place_repeat(
    db: &Connection,
    card: &str,
    previous_kind: Option<&str>,
    now: i64,
    draw: u8,
) -> Result<Option<u8>> {
    place_repeat_bounded(db, card, previous_kind, now, draw, None)
}

fn place_repeat_bounded(
    db: &Connection,
    card: &str,
    previous_kind: Option<&str>,
    now: i64,
    draw: u8,
    write_budget: Option<u64>,
) -> Result<Option<u8>> {
    db.execute_batch("SAVEPOINT repeat_placement")?;
    match fill_repeat(db, card, previous_kind, now, draw, write_budget) {
        Ok(offset) => {
            db.execute_batch("RELEASE repeat_placement")?;
            Ok(Some(offset))
        }
        Err(error) => {
            let interrupted = error
                .downcast_ref::<rusqlite::Error>()
                .and_then(rusqlite::Error::sqlite_error_code)
                == Some(rusqlite::ErrorCode::OperationInterrupted);
            if db.is_autocommit() || interrupted {
                return Err(error);
            }
            db.execute_batch("ROLLBACK TO repeat_placement; RELEASE repeat_placement")?;
            if error.is::<Preparing>() {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

fn fill_repeat(
    db: &Connection,
    card: &str,
    previous_kind: Option<&str>,
    now: i64,
    draw: u8,
    write_budget: Option<u64>,
) -> Result<u8> {
    let mut q = db.prepare(
        "SELECT d.slot,p.kind,p.kind='opening' AND p.due_at IS NULL
         FROM deck d CROSS JOIN prepared_queue p ON p.card_id=d.card_id
         WHERE p.active=1 ORDER BY d.slot LIMIT 22",
    )?;
    let mut queued = q
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    anyhow::ensure!(
        queued.len() <= 20,
        "repeat deck exceeds its supported bound"
    );
    // Only the prefix needed for the actual draw must be certified. A later
    // held opening cannot affect insertion ahead of it.
    if queued
        .iter()
        .take(usize::from(draw))
        .any(|(_, _, unseen_opening)| *unseen_opening)
    {
        anyhow::ensure!(daily_store::opening_commitments_ready(db, now)?, Preparing);
    }
    let mut previous = queued
        .last()
        .map(|(_, kind, _)| kind.clone())
        .or_else(|| previous_kind.map(str::to_owned));
    // Tactics can place a repeat while Study is still waiting on a different card.
    let unanswered = crate::study_store::unanswered_card(db)?;
    let protected = if let Some(id) = &unanswered {
        check_repeat_budget(db, write_budget, 1)?;
        db.execute(
            "UPDATE prepared_queue SET reserved=1 WHERE card_id=?1 AND reserved=0",
            [id],
        )? != 0
    } else {
        false
    };
    while queued.len() < usize::from(draw) {
        // An existing day has at most two exact due deletions and size triggers,
        // one commitment, day counters/mix and two deck reservation writes.
        check_repeat_budget(db, write_budget, 12)?;
        let Some((id, kind)) = daily_store::take_fresh(db, previous.as_deref(), now)? else {
            break;
        };
        let slot = queued.last().map_or(1, |(slot, _, _)| slot + 1);
        db.execute("INSERT INTO deck VALUES (?1,?2)", params![slot, id])?;
        previous = Some(kind.clone());
        queued.push((slot, kind, false));
    }
    if protected {
        check_repeat_budget(db, write_budget, 1)?;
        db.execute(
            "UPDATE prepared_queue SET reserved=0 WHERE card_id=?1",
            [unanswered],
        )?;
    }
    let offset = queue::repeat_offset(draw, queued.len());
    check_repeat_budget(
        db,
        write_budget,
        (queued.len() - usize::from(offset)) as u64 + 2,
    )?;
    let slot = if let Some((slot, _, _)) = queued.get(usize::from(offset)) {
        for (position, _, _) in queued[usize::from(offset)..].iter().rev() {
            db.execute(
                "UPDATE deck SET slot=?2 WHERE slot=?1",
                params![position, position + 1],
            )?;
        }
        *slot
    } else {
        queued.last().map_or(1, |(slot, _, _)| slot + 1)
    };
    db.execute("INSERT INTO deck VALUES (?1,?2)", params![slot, card])?;
    Ok(offset)
}

/// Every seeded row and its cursor are committed together. Ordinary writes maintain
/// already-seeded entries in their own transactions, including inserts behind the cursor.
pub fn seed_batch(db: &mut Connection) -> Result<bool> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (ready, cursor): (bool, String) = tx.query_row(
        "SELECT ready,cursor FROM prepared_queue_state WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if ready {
        return Ok(true);
    }
    // The projection insert and its two version updates use three changes per card.
    const SEED_LIMIT: usize = 42;
    let ids = {
        let mut query = tx.prepare("SELECT id FROM cards WHERE id>?1 ORDER BY id LIMIT ?2")?;
        query
            .query_map((cursor, SEED_LIMIT), |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut insert = tx.prepare_cached(
        "INSERT OR IGNORE INTO prepared_queue(card_id,kind,active,due_at,rank,reserved,capture_order)
         SELECT c.id,c.kind,active.id IS NOT NULL,l.due_at,o.rank,d.card_id IS NOT NULL OR EXISTS(SELECT 1 FROM repeat_obligation r WHERE r.card_id=c.id),c.capture_order
         FROM cards c LEFT JOIN active_cards active ON active.id=c.id
         LEFT JOIN learning l ON l.card_id=c.id LEFT JOIN opening_order o ON o.card_id=c.id LEFT JOIN deck d ON d.card_id=c.id
         WHERE c.id=?1",
    )?;
    for id in &ids {
        insert.execute([id])?;
    }
    drop(insert);
    let complete = ids.len() < SEED_LIMIT;
    tx.execute(
        "UPDATE prepared_queue_state SET ready=?1,cursor=coalesce(?2,cursor) WHERE singleton=1",
        params![complete, ids.last()],
    )?;
    tx.commit()?;
    Ok(complete)
}

pub fn prepared(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT ready FROM prepared_queue_state WHERE singleton=1",
        [],
        |row| row.get(0),
    )?)
}

pub fn defer_repeat(
    db: &Connection,
    attempt: i64,
    card: &str,
    previous: Option<&str>,
    now: i64,
    draw: u8,
) -> Result<()> {
    db.execute(
        "INSERT INTO repeat_obligation VALUES (?1,?2,?3,?4,?5)",
        params![attempt, card, previous, now, draw],
    )?;
    Ok(())
}

fn has_repeat_space(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM deck LIMIT 1 OFFSET 20)",
        [],
        |r| r.get(0),
    )?)
}
pub fn can_place_now(db: &Connection) -> Result<bool> {
    Ok(prepared(db)?
        && has_repeat_space(db)?
        && db.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM repeat_obligation)",
            [],
            |r| r.get::<_, bool>(0),
        )?)
}
pub fn place_deferred(db: &Connection, now: i64) -> Result<()> {
    if !has_repeat_space(db)? {
        return Ok(());
    }
    let obligation = db.query_row(
        "SELECT o.attempt_id,o.card_id,o.previous_kind,o.draw FROM repeat_order q JOIN repeat_obligation o USING(attempt_id) ORDER BY q.seq LIMIT 1", [],
        |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,Option<String>>(2)?,row.get::<_,u8>(3)?)),
    ).optional()?;
    let Some((attempt, card, previous, draw)) = obligation else {
        return Ok(());
    };
    if can_repeat(db, &card)? && place_repeat(db, &card, previous.as_deref(), now, draw)?.is_none()
    {
        return Ok(());
    }
    db.execute(
        "DELETE FROM repeat_obligation WHERE attempt_id=?1",
        [attempt],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::repeat_offset;

    #[test]
    fn full_deck_defers_multiple_repeats_in_acceptance_order() -> Result<()> {
        let files = tempfile::tempdir()?;
        let mut db = crate::db::Database::init(&files.path().join("tempo.sqlite3"))?;
        db.execute_batch(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<23)
            INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
             SELECT printf('t%02d',i),'tactic','','white','',printf('t%02d',i),'[]' FROM n;
            INSERT INTO deck SELECT row_number() OVER (ORDER BY id),id FROM cards WHERE id<'t22';
            INSERT INTO learning VALUES ('t22',1,1,1,2),('t23',1,1,1,2);
            INSERT INTO attempts(id,card_id,result) VALUES (2,'t22','saved'),(1,'t23','saved');",
        )?;
        while !seed_batch(&mut db)? {}
        daily_store::prepare_test_day(&mut db, 3)?;
        let tx = db.transaction()?;
        defer_repeat(&tx, 2, "t22", None, 1, 5)?;
        defer_repeat(&tx, 1, "t23", None, 1, 5)?;
        place_deferred(&tx, 3)?;
        assert!(!can_place_now(&tx)?);
        assert_eq!(
            tx.query_row("SELECT count(*) FROM repeat_order", [], |r| r
                .get::<_, i64>(0))?,
            2
        );
        consume(&tx, "t01")?;
        place_deferred(&tx, 3)?;
        assert!(tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM deck WHERE card_id='t22')",
            [],
            |r| r.get::<_, bool>(0)
        )?);
        assert_eq!(
            tx.query_row("SELECT card_id FROM repeat_order", [], |r| r
                .get::<_, String>(0))?,
            "t23"
        );
        consume(&tx, "t02")?;
        place_deferred(&tx, 3)?;
        assert!(tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM deck WHERE card_id='t23')",
            [],
            |r| r.get::<_, bool>(0)
        )?);
        assert_eq!(
            tx.query_row("SELECT count(*) FROM repeat_order", [], |r| r
                .get::<_, i64>(0))?,
            0
        );
        tx.commit()?;
        Ok(())
    }

    #[test]
    fn counts_read_published_work_and_keep_its_cutoff() -> Result<()> {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        use serde_json::json;

        let files = tempfile::tempdir()?;
        let mut db = crate::db::Database::init(&files.path().join("tempo.sqlite3"))?;
        db.execute_batch(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<25)
             INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
             SELECT printf('t%02d',i),'tactic','','white','',printf('t%02d',i),'[]' FROM n;
             INSERT INTO learning VALUES ('t01',1,1,1,10), ('t02',1,1,1,11);",
        )?;
        while !seed_batch(&mut db)? {}
        daily_store::prepare_test_day(&mut db, 10)?;
        let changes = db.total_changes();
        db.flush_prepared_statement_cache();
        db.authorizer(Some(|context: AuthContext<'_>| match context.action {
            AuthAction::Read {
                table_name: "cards" | "learning" | "games" | "opening_order",
                ..
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }));
        for now in [10, 11] {
            let totals = daily_store::counts(&db, now, None)?.expect("published daily plan");
            assert_eq!(totals["new"], json!(20));
            assert_eq!(totals["reviews"], json!(1));
        }
        assert_eq!(db.total_changes(), changes);
        Ok(())
    }

    #[test]
    fn seed_progress_observes_interleaved_changes_even_behind_the_cursor() -> Result<()> {
        let files = tempfile::tempdir()?;
        let mut db = crate::db::Database::init(&files.path().join("seed.sqlite3"))?;
        for id in 1..=130 {
            db.execute(
                "INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
                VALUES (?1,'tactic','','white','','','[]')",
                [format!("{id:04}")],
            )?;
        }
        // Simulate retained cards awaiting prepared projection population.
        db.execute("DELETE FROM prepared_queue", [])?;
        assert!(!seed_batch(&mut db)?);
        assert!(!prepared(&db)?);
        db.execute("INSERT INTO learning VALUES ('0001',1,1,1,9)", [])?;
        db.execute(
            "INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
            VALUES ('0000','tactic','','white','','','[]')",
            [],
        )?;
        db.execute("INSERT INTO deck VALUES (1,'0000')", [])?;
        db.execute(
            "UPDATE cards SET capture_order=(SELECT sequence+1 FROM daily_clock) WHERE id='0130'",
            [],
        )?;
        while !seed_batch(&mut db)? {}
        assert_eq!(
            db.query_row("SELECT count(*) FROM prepared_queue", [], |r| r
                .get::<_, usize>(0))?,
            131
        );
        assert_eq!(
            db.query_row(
                "SELECT due_at FROM prepared_queue WHERE card_id='0001'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            9
        );
        assert!(db.query_row(
            "SELECT p.capture_order=c.capture_order FROM prepared_queue p JOIN cards c ON c.id=p.card_id
             WHERE p.card_id='0130'",
            [],
            |r| r.get::<_, bool>(0),
        )?);
        let reserved = || {
            db.query_row(
                "SELECT reserved FROM prepared_queue WHERE card_id='0000'",
                [],
                |r| r.get::<_, bool>(0),
            )
        };
        assert!(reserved()?);
        consume(&db, "0000")?;
        assert!(!reserved()?);
        Ok(())
    }

    #[test]
    fn daily_limits_use_the_opening_settings_owner() -> Result<()> {
        let files = tempfile::tempdir()?;
        let db = crate::db::Database::init(&files.path().join("tempo.sqlite3"))?;
        let defaults = new_opening_settings(&db)?;
        assert_eq!((defaults.opening_limit, defaults.tactic_limit), (20, 20));
        let saved = save_new_opening_settings(
            &db,
            UpdateNewOpeningSettings {
                paused: true,
                ordering: NewOpeningOrder::Standard,
                opening_limit: Some(7),
                tactic_limit: Some(0),
            },
        )?;
        assert_eq!(
            serde_json::to_value(new_opening_settings(&db)?)?,
            serde_json::to_value(saved)?
        );
        let legacy = serde_json::from_str(r#"{"paused":false,"ordering":"maia"}"#)?;
        let updated = save_new_opening_settings(&db, legacy)?;
        assert!(!updated.paused);
        assert_eq!(updated.ordering.as_str(), "maia");
        assert_eq!((updated.opening_limit, updated.tactic_limit), (7, 0));
        let read_back = serde_json::to_value(new_opening_settings(&db)?)?;
        let rejected = UpdateNewOpeningSettings {
            paused: true,
            ordering: NewOpeningOrder::Standard,
            opening_limit: None,
            tactic_limit: Some(21),
        };
        assert!(save_new_opening_settings(&db, rejected).is_err());
        assert_eq!(read_back, serde_json::to_value(new_opening_settings(&db)?)?);
        Ok(())
    }

    #[test]
    fn spacing_clamps_to_the_remaining_deck() {
        assert_eq!(repeat_offset(5, 20), 5);
        assert_eq!(repeat_offset(20, 20), 20);
        assert_eq!(repeat_offset(20, 3), 3);
        assert_eq!(repeat_offset(5, 0), 0);
    }
}
