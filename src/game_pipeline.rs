// Application orchestration. Games archives, the scanner compares, and Tactics publishes.
use crate::{
    catalog, chess,
    games::{self, Game},
    scanner,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior::Immediate, params};

fn stage(db: &Connection, parent: i64, operation: &str) -> Result<i64> {
    db.execute(
        "INSERT INTO jobs(provider,username,operation,status,parent_id)
         VALUES ('archive','',?1,'queued',?2) ON CONFLICT(parent_id,operation) DO NOTHING",
        params![operation, parent],
    )?;
    Ok(db.query_row(
        "SELECT id FROM jobs WHERE parent_id=?1 AND operation=?2",
        params![parent, operation],
        |r| r.get(0),
    )?)
}

fn enqueue(db: &Connection, parent: i64, operation: &str, game: &str) -> Result<()> {
    let job = stage(db, parent, operation)?;
    if db.execute(
        "INSERT OR IGNORE INTO job_games(job_id,game_id) VALUES (?1,?2)",
        params![job, game],
    )? == 1
    {
        // An unfinished parent can resume after its earlier child batch completed.
        db.execute(
            "UPDATE jobs SET status='queued' WHERE id=?1 AND status='done'",
            [job],
        )?;
    }
    Ok(())
}

pub fn archive_import(db: &mut Connection, game: &Game, job: i64) -> Result<()> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let added = games::accept_game(&tx, game)?;
    if tx.execute(
        "INSERT OR IGNORE INTO job_games(job_id,game_id,scanned) VALUES (?1,?2,1)",
        params![job, game.id],
    )? == 1
    {
        tx.execute(
            "UPDATE jobs SET added=added+?1,existing=existing+?2 WHERE id=?3",
            params![added, !added, job],
        )?;
        if added {
            enqueue(&tx, job, "scan_new", &game.id)?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn pending_games(db: &Connection, job: i64) -> Result<Vec<String>> {
    let mut query = db.prepare(
        "SELECT game_id FROM job_games WHERE job_id=?1 AND scanned=0 ORDER BY game_id LIMIT 25",
    )?;
    Ok(query
        .query_map([job], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn scan_game(db: &mut Connection, job: i64, game: &str) -> Result<()> {
    let snapshot = db.transaction()?;
    if snapshot
        .query_row(
            "SELECT scanned FROM job_games WHERE job_id=?1 AND game_id=?2",
            params![job, game],
            |row| row.get::<_, bool>(0),
        )
        .optional()?
        == Some(true)
    {
        return Ok(());
    }
    let archived = games::read(&snapshot, game)?;
    let results = scanner::scan(
        &archived.id,
        &archived.route,
        &archived.learner_color,
        |position, color| catalog::answers_at(&snapshot, position, color),
    )?;
    snapshot.commit()?;
    let tx = db.transaction_with_behavior(Immediate)?;
    for event in results.mismatches {
        tx.execute(
            "INSERT OR IGNORE INTO mismatches VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                event.game_id,
                event.ply,
                event.position_id,
                event.repertoire_id,
                event.answer_version,
                event.played_move
            ],
        )?;
    }
    let mut found = 0;
    for event in results.captures {
        found += tx.execute(
            "INSERT OR IGNORE INTO missed_captures(game_id,ply,position_id,played_move,captures,fen)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![game,event.ply,event.position_id,event.played_move,serde_json::to_string(&event.captures)?,event.fen],
        )?;
    }
    if found > 0 {
        enqueue(&tx, job, "publish", game)?;
    }
    tx.execute(
        "UPDATE jobs SET found=found+?1 WHERE id=?2",
        params![found, job],
    )?;
    complete(&tx, job, game)?;
    tx.commit()?;
    Ok(())
}

fn complete(db: &Connection, job: i64, game: &str) -> Result<()> {
    db.execute(
        "INSERT INTO job_games(job_id,game_id,scanned) VALUES (?1,?2,1)
         ON CONFLICT(job_id,game_id) DO UPDATE SET scanned=1",
        params![job, game],
    )?;
    Ok(())
}

pub fn publish_game(db: &mut Connection, job: i64, game: &str) -> Result<()> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let mut published = 0;
    {
        let mut query = tx.prepare(
            "SELECT ply,fen,position_id,played_move,captures FROM missed_captures m
             WHERE game_id=?1 AND NOT EXISTS (
               SELECT 1 FROM tactics t WHERE t.game_id=m.game_id AND t.game_ply=m.ply
             ) ORDER BY ply",
        )?;
        let mut rows = query.query([game])?;
        while let Some(row) = rows.next()? {
            let ply: usize = row.get(0)?;
            let fen: String = row.get(1)?;
            let position: String = row.get(2)?;
            let played: String = row.get(3)?;
            let answers: Vec<String> = serde_json::from_str(row.get_ref(4)?.as_str()?)?;
            let first = answers
                .first()
                .context("missed capture has no accepted move")?;
            ensure!(
                chess::position_id(&chess::position(&fen)?) == position,
                "saved capture position changed"
            );
            catalog::publish_tactic(
                &tx,
                catalog::TacticInput {
                    name: &format!("Missed capture · move {}", ply / 2 + 1),
                    route: &chess::Route {
                        fen,
                        moves: vec![first.clone()],
                    },
                    answers: &answers,
                    source: Some((game, ply, &played)),
                },
            )?;
            published += 1;
        }
    }
    tx.execute(
        "UPDATE jobs SET found=found+?1 WHERE id=?2",
        params![published, job],
    )?;
    complete(&tx, job, game)?;
    tx.commit()?;
    Ok(())
}

/// Transfer unfinished pre-split work in the schema upgrade transaction.
pub fn recover_legacy(db: &Connection) -> Result<()> {
    let mut query = db.prepare(
        "SELECT j.id,jg.game_id FROM job_games jg JOIN jobs j ON j.id=jg.job_id
         WHERE j.provider!='archive' AND jg.scanned=0 ORDER BY j.id,jg.game_id",
    )?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        enqueue(db, row.get(0)?, "scan_new", row.get_ref(1)?.as_str()?)?;
    }
    let mut query = db.prepare(
        "SELECT m.game_id,m.ply,m.position_id FROM missed_captures m
         WHERE NOT EXISTS(SELECT 1 FROM tactics t WHERE t.game_id=m.game_id AND t.game_ply=m.ply)
         ORDER BY m.game_id,m.ply",
    )?;
    let mut rows = query.query([])?;
    let mut batch = None;
    while let Some(row) = rows.next()? {
        let game: String = row.get(0)?;
        let ply: usize = row.get(1)?;
        let position: String = row.get(2)?;
        let board = chess::board_at(&games::read(db, &game)?.route, ply)?;
        ensure!(
            chess::position_id(&board) == position,
            "legacy missed capture position changed"
        );
        db.execute(
            "UPDATE missed_captures SET fen=?3 WHERE game_id=?1 AND ply=?2",
            params![game, ply, chess::fen(&board)],
        )?;
        let job = match batch {
            Some(job) => job,
            None => {
                db.execute("INSERT INTO jobs(provider,username,operation,status) VALUES ('archive','','publish','queued')", [])?;
                let id = db.last_insert_rowid();
                batch = Some(id);
                id
            }
        };
        db.execute(
            "INSERT OR IGNORE INTO job_games(job_id,game_id) VALUES (?1,?2)",
            params![job, game],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_publication_uses_saved_capture_and_leaves_archive_and_scan_complete() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        db.execute_batch(include_str!("schema.sql"))?;
        db.execute_batch(include_str!("opening_policy.sql"))?;
        db.execute_batch(include_str!("game_pipeline.sql"))?;
        db.execute("INSERT INTO jobs(provider,username,operation,status) VALUES ('pgn','Learner','upload','done')", [])?;
        let text = "[White \"Learner\"]\n[Black \"Opponent\"]\n[Result \"1/2-1/2\"]\n[SetUp \"1\"]\n[FEN \"4k3/8/8/3n4/8/8/8/3RK3 w - - 0 1\"]\n\n1. Ke2 1/2-1/2";
        let game = games::archive_game(chess::parse_pgn(text, false, 1)?.remove(0)?, "Learner")?;
        archive_import(&mut db, &game, 1)?;
        archive_import(&mut db, &game, 1)?;
        let scan = stage(&db, 1, "scan_new")?;
        assert_eq!(pending_games(&db, scan)?, [game.id.clone()]);
        scan_game(&mut db, scan, &game.id)?;
        scan_game(&mut db, scan, &game.id)?;
        let publish = stage(&db, scan, "publish")?;
        db.execute_batch("CREATE TRIGGER fail_publish BEFORE INSERT ON tactics BEGIN SELECT raise(abort,'fixture failure'); END;")?;
        assert!(publish_game(&mut db, publish, &game.id).is_err());
        assert!(pending_games(&db, scan)?.is_empty());
        assert_eq!(pending_games(&db, publish)?, [game.id.clone()]);
        assert_eq!(
            db.query_row("SELECT count(*) FROM missed_captures", [], |r| r
                .get::<_, usize>(0))?,
            1
        );
        db.execute_batch("DROP TRIGGER fail_publish;")?;
        publish_game(&mut db, publish, &game.id)?;
        publish_game(&mut db, publish, &game.id)?;
        assert!(pending_games(&db, publish)?.is_empty());
        assert_eq!(catalog::tactics(&db, "")?.len(), 1);
        Ok(())
    }
}
