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
    let due = "SELECT l.card_id FROM learning l JOIN active_cards c ON c.id=l.card_id
        WHERE l.due_at<=?1 AND c.kind=?2 AND NOT EXISTS (SELECT 1 FROM deck d WHERE d.card_id=l.card_id)
        ORDER BY l.due_at,l.card_id LIMIT 1";
    let unseen = "SELECT c.id FROM active_cards c WHERE c.kind=?1 AND NOT EXISTS (SELECT 1 FROM learning l WHERE l.card_id=c.id)
        AND NOT EXISTS (SELECT 1 FROM deck d WHERE d.card_id=c.id) ORDER BY c.id LIMIT 1";
    let ranked = "SELECT o.card_id FROM opening_order o CROSS JOIN active_cards c ON c.id=o.card_id
                     WHERE c.kind='opening'
                     AND NOT EXISTS (SELECT 1 FROM learning l WHERE l.card_id=o.card_id)
                     AND NOT EXISTS (SELECT 1 FROM deck d WHERE d.card_id=o.card_id)
                     ORDER BY o.rank LIMIT 1";
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

pub fn place_repeat(db: &Connection, card: &str, now: i64, draw: u8) -> Result<u8> {
    let mut q = db.prepare(
        "SELECT d.card_id,c.kind FROM deck d JOIN active_cards c ON c.id=d.card_id ORDER BY d.slot",
    )?;
    let mut queued = q
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut previous = if let Some((_, kind)) = queued.last() {
        Some(kind.clone())
    } else {
        db.query_row("SELECT last_kind FROM study", [], |r| r.get(0))?
    };
    while queued.len() < usize::from(draw) {
        let Some((id, kind)) = select(db, previous.as_deref(), now)? else {
            break;
        };
        db.execute(
            "INSERT INTO deck SELECT coalesce(max(slot),0)+1,?1 FROM deck",
            [&id],
        )?;
        previous = Some(kind.clone());
        queued.push((id, kind));
    }
    let offset = queue::repeat_offset(draw, queued.len());
    db.execute("DELETE FROM deck", [])?;
    let mut ids = queued.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
    ids.insert(usize::from(offset), card.to_string());
    for (slot, id) in ids.into_iter().enumerate() {
        db.execute("INSERT INTO deck VALUES (?1,?2)", params![slot, id])?;
    }
    Ok(offset)
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
