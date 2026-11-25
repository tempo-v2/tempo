use crate::queue::{self, Candidates, CardKind, Counts, NewOpeningOrder, NewOpeningSettings};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

const PREPARED_UNSEEN: &str = "FROM prepared_queue
    WHERE active=1 AND reserved=0 AND due_at IS NULL AND kind=?1";
const PREPARED_DUE: &str = "FROM prepared_queue
    WHERE active=1 AND reserved=0 AND due_at IS NOT NULL AND kind=?1 AND due_at<=?2";

fn card_kind(kind: &str) -> Result<CardKind> {
    match kind {
        "opening" => Ok(CardKind::Opening),
        "tactic" => Ok(CardKind::Tactic),
        _ => anyhow::bail!("Unknown prepared card kind"),
    }
}

pub fn new_opening_settings(db: &Connection) -> Result<NewOpeningSettings> {
    let (paused, ordering): (bool, String) = db.query_row(
        "SELECT paused, ordering FROM new_opening_settings WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let ordering = match ordering.as_str() {
        "maia" => NewOpeningOrder::SavedRank,
        "standard" => NewOpeningOrder::Standard,
        _ => anyhow::bail!("Unknown new opening order"),
    };
    Ok(NewOpeningSettings { paused, ordering })
}

pub fn save_new_opening_settings(
    db: &Connection,
    preferences: NewOpeningSettings,
) -> Result<NewOpeningSettings> {
    db.execute(
        "UPDATE new_opening_settings SET paused=?1, ordering=?2 WHERE singleton=1",
        params![preferences.paused, preferences.ordering.as_str()],
    )?;
    Ok(preferences)
}

pub fn select(
    db: &Connection,
    previous: Option<&str>,
    now: i64,
) -> Result<Option<(String, String)>> {
    let previous = previous.map(card_kind).transpose()?;
    let chosen = queue::select(
        previous,
        || new_opening_settings(db),
        |kind, candidates| {
            Ok(match candidates {
                Candidates::Due if kind==CardKind::Opening => return due_opening(db,now),
                Candidates::Due => db
                    .prepare_cached(&format!(
                        "SELECT card_id {PREPARED_DUE} ORDER BY due_at,card_id LIMIT 1"
                    ))?
                    .query_row(params![kind.as_str(), now], |r| r.get(0))
                    .optional(),
                Candidates::New => db
                    .prepare_cached(&format!(
                        "SELECT card_id {PREPARED_UNSEEN} ORDER BY card_id LIMIT 1"
                    ))?
                    .query_row([kind.as_str()], |r| r.get(0))
                    .optional(),
                Candidates::NewByRank => db
                    .prepare_cached(&format!(
                        "SELECT card_id {PREPARED_UNSEEN} AND rank IS NOT NULL ORDER BY rank,card_id LIMIT 1"
                    ))?
                    .query_row([kind.as_str()], |r| r.get(0))
                    .optional(),
            }?)
        },
    )?;
    Ok(chosen.map(|(id, kind)| (id, kind.as_str().to_owned())))
}

// The earliest due slot determines which applied forecast, if any, can reorder it.
fn due_opening(db: &Connection, now: i64) -> Result<Option<String>> {
    let head: Option<(String, Option<String>)> = db
        .prepare_cached(&format!(
            "SELECT card_id,forecast_group {PREPARED_DUE} ORDER BY due_at,card_id LIMIT 1"
        ))?
        .query_row(params!["opening", now], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let Some((id, group)) = head else {
        return Ok(None);
    };
    let Some(group) = group else {
        return Ok(Some(id));
    };
    Ok(Some(
        db.prepare_cached(
            "SELECT card_id FROM prepared_queue
        WHERE forecast_group=?1 AND active=1 AND reserved=0 AND due_at IS NOT NULL AND due_at<=?2
        ORDER BY rank,card_id LIMIT 1",
        )?
        .query_row(params![group, now], |r| r.get(0))?,
    ))
}

pub fn can_repeat(db: &Connection, card: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM active_cards WHERE id=?1)",
        [card],
        |r| r.get(0),
    )?)
}

/// Count prepared candidates and committed work in the caller's read snapshot.
/// A missing result means initial membership preparation is still in progress.
pub fn counts(db: &Connection, now: i64, unanswered: Option<&str>) -> Result<Option<Counts>> {
    if !prepared(db)? {
        return Ok(None);
    }
    let paused = db.query_row(
        "SELECT paused FROM new_opening_settings WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    let mut counts = Counts::default();
    // One additional match distinguishes an exact total at the cap from a lower bound.
    const SCAN_LIMIT: usize = queue::COUNT_CAP + 1;
    let mut unseen = db.prepare(&format!(
        "SELECT count(*) FROM (SELECT 1 {PREPARED_UNSEEN} LIMIT {SCAN_LIMIT})"
    ))?;
    let mut due = db.prepare(&format!(
        "SELECT count(*) FROM (SELECT 1 {PREPARED_DUE} LIMIT {SCAN_LIMIT})"
    ))?;
    for kind in [CardKind::Opening, CardKind::Tactic] {
        if queue::admits_uncommitted_new(kind, paused) {
            counts.new += unseen.query_row([kind.as_str()], |row| row.get::<_, usize>(0))?;
        }
        counts.reviews +=
            due.query_row(params![kind.as_str(), now], |row| row.get::<_, usize>(0))?;
    }
    // Again can add one card to the 20-slot spacing window. Without prepared
    // membership, Next cannot issue another attempt while a repeat is deferred.
    let exceeds_committed_bound = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM deck LIMIT 1 OFFSET 21)
            OR EXISTS(SELECT 1 FROM repeat_obligation LIMIT 1 OFFSET 1)",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    anyhow::ensure!(
        !exceeds_committed_bound,
        "Committed work exceeds the Study count bound"
    );
    // SQLite preserves CROSS JOIN order. Drive the lookups from committed
    // identities so the optimizer cannot scan the entire prepared queue.
    let mut committed = db.prepare(
        "WITH committed(card_id) AS (
            SELECT card_id FROM deck
            UNION SELECT card_id FROM repeat_obligation
            UNION SELECT ?1 WHERE ?1 IS NOT NULL
        )
        SELECT q.kind,q.due_at,q.reserved FROM committed c
        CROSS JOIN prepared_queue q ON q.card_id=c.card_id WHERE q.active=1",
    )?;
    let mut rows = committed.query([unanswered])?;
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        counts.include_committed(card_kind(&kind)?, row.get(1)?, row.get(2)?, paused, now);
    }
    Ok(Some(counts))
}

pub fn committed_head(db: &Connection) -> Result<Option<(String, String)>> {
    Ok(db.query_row(
        "SELECT d.card_id,c.kind FROM deck d CROSS JOIN active_cards c ON c.id=d.card_id ORDER BY d.slot LIMIT 1",
        [], |r| Ok((r.get(0)?,r.get(1)?)),
    ).optional()?)
}

pub fn consume(db: &Connection, card: &str) -> Result<()> {
    db.execute("DELETE FROM deck WHERE card_id=?1", [card])?;
    Ok(())
}

pub fn place_repeat(
    db: &Connection,
    card: &str,
    previous_kind: Option<&str>,
    now: i64,
    draw: u8,
) -> Result<u8> {
    let mut q = db.prepare(
        "SELECT d.slot,c.kind FROM deck d CROSS JOIN active_cards c ON c.id=d.card_id ORDER BY d.slot LIMIT 22",
    )?;
    let mut queued = q
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    anyhow::ensure!(
        queued.len() <= 20,
        "repeat deck exceeds its supported bound"
    );
    let mut previous = queued
        .last()
        .map(|(_, kind)| kind.clone())
        .or_else(|| previous_kind.map(str::to_owned));
    while queued.len() < usize::from(draw) {
        let Some((id, kind)) = select(db, previous.as_deref(), now)? else {
            break;
        };
        let slot = queued.last().map_or(1, |(slot, _)| slot + 1);
        db.execute("INSERT INTO deck VALUES (?1,?2)", params![slot, id])?;
        previous = Some(kind.clone());
        queued.push((slot, kind));
    }
    let offset = queue::repeat_offset(draw, queued.len());
    let slot = if let Some((slot, _)) = queued.get(usize::from(offset)) {
        for (position, _) in queued[usize::from(offset)..].iter().rev() {
            db.execute(
                "UPDATE deck SET slot=?2 WHERE slot=?1",
                params![position, position + 1],
            )?;
        }
        *slot
    } else {
        queued.last().map_or(1, |(slot, _)| slot + 1)
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
    let ids = {
        let mut query = tx.prepare("SELECT id FROM cards WHERE id>?1 ORDER BY id LIMIT 127")?;
        query
            .query_map([cursor], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut insert = tx.prepare_cached(
        "INSERT OR IGNORE INTO prepared_queue(card_id,kind,active,due_at,rank,reserved)
         SELECT c.id,c.kind,NOT c.quarantined,l.due_at,o.rank,d.card_id IS NOT NULL
         FROM cards c LEFT JOIN learning l ON l.card_id=c.id
         LEFT JOIN opening_order o ON o.card_id=c.id LEFT JOIN deck d ON d.card_id=c.id
         WHERE c.id=?1",
    )?;
    for id in &ids {
        insert.execute([id])?;
    }
    drop(insert);
    let complete = ids.len() < 127;
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

pub fn place_deferred(db: &Connection) -> Result<()> {
    let obligation = db.query_row(
        "SELECT attempt_id,card_id,previous_kind,accepted_at,draw FROM repeat_obligation ORDER BY attempt_id LIMIT 1", [],
        |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,Option<String>>(2)?,row.get::<_,i64>(3)?,row.get::<_,u8>(4)?)),
    ).optional()?;
    let Some((attempt, card, previous, now, draw)) = obligation else {
        return Ok(());
    };
    if can_repeat(db, &card)? {
        place_repeat(db, &card, previous.as_deref(), now, draw)?;
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
    fn counts_respect_readiness_pause_due_time_and_committed_work() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        for schema in [
            include_str!("schema.sql"),
            include_str!("opening_policy.sql"),
            include_str!("opening_order.sql"),
            include_str!("game_pipeline.sql"),
            include_str!("prepared_queue.sql"),
        ] {
            db.execute_batch(schema)?;
        }
        db.execute_batch(
            "INSERT INTO cards(id,kind,title,color,fen,position_id,answers,quarantined) VALUES
                ('paused','opening','','white','','paused','[]',0),
                ('queued-new','opening','','white','','queued-new','[]',0),
                ('issued-new','opening','','white','','issued-new','[]',0),
                ('inactive','opening','','white','','inactive','[]',1),
                ('new','tactic','','white','','new','[]',0),
                ('due','tactic','','white','','due','[]',0),
                ('future','tactic','','white','','future','[]',0),
                ('repeat','tactic','','white','','repeat','[]',0),
                ('deferred','tactic','','white','','deferred','[]',0);
             INSERT INTO learning VALUES ('due',1,1,1,10), ('future',1,1,1,11),
                 ('repeat',1,1,1,20), ('deferred',1,1,1,20);
             INSERT INTO deck VALUES (1,'queued-new'), (2,'repeat'), (3,'due');
             INSERT INTO attempts(id,card_id,result) VALUES (1,'deferred','saved');
             INSERT INTO repeat_obligation VALUES (1,'deferred','tactic',1,5);
             UPDATE new_opening_settings SET paused=1;",
        )?;
        assert_eq!(counts(&db, 10, Some("issued-new"))?, None);
        assert!(seed_batch(&mut db)?);
        let changes: u64 = db.total_changes();
        assert_eq!(
            counts(&db, 10, Some("issued-new"))?,
            Some(Counts { new: 3, reviews: 3 })
        );
        assert_eq!(
            counts(&db, 11, Some("repeat"))?,
            Some(Counts { new: 2, reviews: 4 })
        );
        assert_eq!(db.total_changes(), changes);
        db.execute(
            "UPDATE new_opening_settings SET paused=0,ordering='standard'",
            [],
        )?;
        assert_eq!(
            counts(&db, 10, Some("issued-new"))?,
            Some(Counts { new: 4, reviews: 3 })
        );
        db.execute("UPDATE new_opening_settings SET ordering='maia'", [])?;
        assert_eq!(
            counts(&db, 10, Some("issued-new"))?,
            Some(Counts { new: 4, reviews: 3 })
        );
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        db.authorizer(Some(|context: AuthContext<'_>| match context.action {
            AuthAction::Read {
                table_name: "cards" | "learning" | "games" | "opening_order",
                ..
            }
            | AuthAction::Read {
                table_name: "new_opening_settings",
                column_name: "ordering",
            }
            | AuthAction::Read {
                table_name: "prepared_queue",
                column_name: "rank",
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }));
        assert_eq!(
            counts(&db, 10, Some("issued-new"))?,
            Some(Counts { new: 4, reviews: 3 })
        );
        db.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);

        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::Relaxed},
        };
        let measure = || -> Result<usize> {
            let steps = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&steps);
            db.progress_handler(
                1,
                Some(move || {
                    counter.fetch_add(1, Relaxed);
                    false
                }),
            );
            let result = counts(&db, 10, Some("issued-new"));
            db.progress_handler(0, None::<fn() -> bool>);
            assert_eq!(result?, Some(Counts { new: 4, reviews: 3 }));
            Ok(steps.load(Relaxed))
        };
        let baseline = measure()?;
        db.execute_batch(
            "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<100000)
             INSERT INTO cards(id,kind,title,color,fen,position_id,answers,quarantined)
             SELECT 'other-'||i, CASE WHEN i%2=0 THEN 'opening' ELSE 'tactic' END,
                    '', 'white', '', '', '[]', i%2=0 FROM n;
             INSERT INTO learning
             SELECT id,1,1,1,1000 FROM cards WHERE id LIKE 'other-%' AND kind='tactic';",
        )?;
        let populated = measure()?;
        assert!(
            populated <= baseline + 100,
            "irrelevant cards grew count work from {baseline} to {populated} SQLite steps"
        );
        Ok(())
    }
    #[test]
    fn counts_preserve_the_cap_boundary_and_bound_eligible_work() -> Result<()> {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::Relaxed},
        };

        let db = Connection::open_in_memory()?;
        for schema in [
            include_str!("schema.sql"),
            include_str!("opening_policy.sql"),
            include_str!("opening_order.sql"),
            include_str!("game_pipeline.sql"),
            include_str!("prepared_queue.sql"),
        ] {
            db.execute_batch(schema)?;
        }
        db.execute_batch(
            "UPDATE prepared_queue_state SET ready=1;
             WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<9999)
             INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
             SELECT 'n-'||i,'opening','','white','','','[]' FROM n
             UNION ALL SELECT 'r-'||i,'tactic','','white','','','[]' FROM n;
             INSERT INTO learning SELECT id,1,1,1,10 FROM cards WHERE kind='tactic';",
        )?;
        let display = |now| -> Result<_> {
            Ok(serde_json::to_value(
                counts(&db, now, None)?.map(queue::DisplayCounts::from),
            )?)
        };
        assert_eq!(display(9)?, serde_json::json!({"new":9999,"reviews":0}));
        assert_eq!(display(10)?, serde_json::json!({"new":9999,"reviews":9999}));
        // A reserved candidate is absent from the bounded base and counted as committed once.
        db.execute_batch(
            "INSERT INTO cards(id,kind,title,color,fen,position_id,answers) VALUES
                 ('committed','opening','','white','','','[]'),
                 ('review-10000','opening','','white','','','[]');
             INSERT INTO deck VALUES (1,'committed');
             INSERT INTO learning VALUES ('review-10000',1,1,1,10);",
        )?;
        assert_eq!(
            display(10)?,
            serde_json::json!({"new":10000,"reviews":10000})
        );
        db.execute_batch(
            "INSERT INTO cards(id,kind,title,color,fen,position_id,answers) VALUES
            ('new-10001','tactic','','white','','','[]');",
        )?;
        assert_eq!(
            display(10)?,
            serde_json::json!({"new":{"at_least":10000},"reviews":10000})
        );
        db.execute_batch(
            "INSERT INTO cards(id,kind,title,color,fen,position_id,answers) VALUES
                ('review-10001','opening','','white','','','[]');
             INSERT INTO learning VALUES ('review-10001',1,1,1,10);",
        )?;
        let capped = serde_json::json!({"new":{"at_least":10000},"reviews":{"at_least":10000}});
        assert_eq!(display(10)?, capped);
        db.execute_batch(
            "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<25000)
             INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
             SELECT 'on-'||i,'opening','','white','','','[]' FROM n
             UNION ALL SELECT 'od-'||i,'opening','','white','','','[]' FROM n
             UNION ALL SELECT 'tn-'||i,'tactic','','white','','','[]' FROM n
             UNION ALL SELECT 'td-'||i,'tactic','','white','','','[]' FROM n;
             INSERT INTO learning SELECT id,1,1,1,10 FROM cards WHERE id GLOB '[ot]d-*';",
        )?;
        let steps = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&steps);
        db.progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, Relaxed);
                false
            }),
        );
        assert_eq!(display(10)?, capped);
        let work = steps.load(Relaxed);
        eprintln!("Study counts SQL steps with all four ranges saturated: {work}");
        assert!(work < 500_000, "Study counts used {work} SQLite steps");
        Ok(())
    }

    #[test]
    fn seed_progress_observes_interleaved_changes_even_behind_the_cursor() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        for schema in [
            include_str!("schema.sql"),
            include_str!("opening_policy.sql"),
            include_str!("opening_order.sql"),
            include_str!("game_pipeline.sql"),
        ] {
            db.execute_batch(schema)?;
        }
        for id in 1..=130 {
            db.execute(
                "INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
                VALUES (?1,'tactic','','white','','','[]')",
                [format!("{id:04}")],
            )?;
        }
        db.execute_batch(include_str!("prepared_queue.sql"))?;
        assert!(!seed_batch(&mut db)?);
        assert!(!prepared(&db)?);
        db.execute("INSERT INTO learning VALUES ('0001',1,1,1,9)", [])?;
        db.execute(
            "INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
            VALUES ('0000','tactic','','white','','','[]')",
            [],
        )?;
        db.execute("INSERT INTO deck VALUES (1,'0000')", [])?;
        assert!(seed_batch(&mut db)?);
        assert_eq!(
            db.query_row("SELECT count(*) FROM prepared_queue", [], |r| r
                .get::<_, usize>(0))?,
            131
        );
        assert_eq!(select(&db, None, 8)?.unwrap().0, "0002");
        assert_eq!(select(&db, None, 9)?.unwrap().0, "0001");
        consume(&db, "0000")?;
        assert_eq!(select(&db, None, 8)?.unwrap().0, "0000");
        Ok(())
    }

    #[test]
    fn saved_opening_order_only_prioritizes_unseen_opening_cards() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(include_str!("schema.sql"))?;
        db.execute_batch(include_str!("opening_policy.sql"))?;
        db.execute_batch(include_str!("opening_order.sql"))?;
        db.execute_batch(include_str!("prepared_queue.sql"))?;
        db.execute_batch(
            "INSERT INTO cards (id,kind,title,color,fen,position_id,answers) VALUES
                ('a','opening','First by id','white','','a','[]'),
                ('b','opening','First by rank','white','','b','[]'),
                ('t','tactic','Tactic','white','','t','[]');
             INSERT INTO opening_order VALUES (1,'b');",
        )?;
        assert_eq!(select(&db, None, 10)?.unwrap().0, "b");
        assert_eq!(select(&db, Some("opening"), 10)?.unwrap().0, "t");

        save_new_opening_settings(
            &db,
            NewOpeningSettings {
                paused: false,
                ordering: NewOpeningOrder::Standard,
            },
        )?;
        assert_eq!(select(&db, None, 10)?.unwrap().0, "a");
        save_new_opening_settings(
            &db,
            NewOpeningSettings {
                paused: true,
                ordering: NewOpeningOrder::SavedRank,
            },
        )?;
        assert_eq!(select(&db, None, 10)?.unwrap().0, "t");
        db.execute("INSERT INTO learning VALUES ('a',2,2,0,5)", [])?;
        assert_eq!(select(&db, None, 10)?.unwrap().0, "a");
        db.execute("DELETE FROM learning", [])?;
        save_new_opening_settings(
            &db,
            NewOpeningSettings {
                paused: false,
                ordering: NewOpeningOrder::SavedRank,
            },
        )?;

        db.execute("UPDATE cards SET quarantined=1 WHERE id='b'", [])?;
        assert_eq!(select(&db, None, 10)?.unwrap().0, "a");
        db.execute("UPDATE cards SET quarantined=0 WHERE id='b'", [])?;

        db.execute("INSERT INTO deck VALUES (0,'b')", [])?;
        assert_eq!(select(&db, None, 10)?.unwrap().0, "a");
        db.execute("DELETE FROM deck", [])?;

        db.execute("INSERT INTO learning VALUES ('a',2,2,0,5)", [])?;
        assert_eq!(select(&db, None, 10)?.unwrap().0, "a");
        db.execute("UPDATE learning SET due_at=20 WHERE card_id='a'", [])?;
        assert_eq!(select(&db, None, 10)?.unwrap().0, "b");
        db.execute("INSERT INTO learning VALUES ('b',2,2,0,20)", [])?;
        assert_eq!(select(&db, None, 10)?.unwrap().0, "t");
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
