use crate::queue::{self, Candidates, CardKind, NewOpeningOrder, NewOpeningSettings};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

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
    let previous = match previous {
        Some("opening") => Some(CardKind::Opening),
        Some("tactic") => Some(CardKind::Tactic),
        None => None,
        Some(_) => anyhow::bail!("Unknown previous card kind"),
    };
    let due = "SELECT card_id FROM prepared_queue
        WHERE active=1 AND reserved=0 AND due_at IS NOT NULL AND due_at<=?1 AND kind=?2
        ORDER BY due_at,card_id LIMIT 1";
    let unseen = "SELECT card_id FROM prepared_queue
        WHERE active=1 AND reserved=0 AND due_at IS NULL AND kind=?1 ORDER BY card_id LIMIT 1";
    let ranked = "SELECT card_id FROM prepared_queue
        WHERE active=1 AND reserved=0 AND due_at IS NULL AND rank IS NOT NULL AND kind='opening'
        ORDER BY rank,card_id LIMIT 1";
    let chosen = queue::select(
        previous,
        || new_opening_settings(db),
        |kind, candidates| {
            Ok(match candidates {
                Candidates::Due => db
                    .query_row(due, params![now, kind.as_str()], |r| r.get(0))
                    .optional(),
                Candidates::New => db
                    .query_row(unseen, [kind.as_str()], |r| r.get(0))
                    .optional(),
                Candidates::NewByRank => db.query_row(ranked, [], |r| r.get(0)).optional(),
            }?)
        },
    )?;
    Ok(chosen.map(|(id, kind)| (id, kind.as_str().to_owned())))
}

pub fn can_repeat(db: &Connection, card: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM active_cards WHERE id=?1)",
        [card],
        |r| r.get(0),
    )?)
}

pub fn committed_head(db: &Connection) -> Result<Option<(String, String)>> {
    Ok(db.query_row(
        "SELECT d.card_id,c.kind FROM deck d JOIN active_cards c ON c.id=d.card_id ORDER BY d.slot LIMIT 1",
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
        "SELECT d.slot,c.kind FROM deck d JOIN active_cards c ON c.id=d.card_id ORDER BY d.slot LIMIT 22",
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
        "INSERT OR IGNORE INTO prepared_queue
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

pub fn place_deferred(db: &Connection) -> Result<bool> {
    let obligation = db.query_row(
        "SELECT attempt_id,card_id,previous_kind,accepted_at,draw FROM repeat_obligation ORDER BY attempt_id LIMIT 1", [],
        |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,Option<String>>(2)?,row.get::<_,i64>(3)?,row.get::<_,u8>(4)?)),
    ).optional()?;
    let Some((attempt, card, previous, now, draw)) = obligation else {
        return Ok(true);
    };
    if !prepared(db)? {
        return Ok(false);
    }
    if can_repeat(db, &card)? {
        place_repeat(db, &card, previous.as_deref(), now, draw)?;
    }
    db.execute(
        "DELETE FROM repeat_obligation WHERE attempt_id=?1",
        [attempt],
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::repeat_offset;
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
