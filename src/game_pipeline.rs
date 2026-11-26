use std::time::Instant;
// Application orchestration. Games archives, the scanner compares, and Tactics publishes.
use crate::{
    catalog, chess,
    game_feedback::{self, Admission, Pair},
    games::{self, Acceptance, Candidate},
    job_store,
    scanner::{self, CaptureProgram, Detector, Observation},
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior::Immediate, params};
use serde::{Deserialize, Serialize};

fn stage(db: &Connection, parent: i64, operation: &str) -> Result<i64> {
    if let Some((detector, _)) = Detector::from_job(operation) {
        db.execute(
            "INSERT INTO jobs(provider,username,operation,status,parent_id,detector_version)
             VALUES ('archive','',?1,'queued',?2,?3) ON CONFLICT(parent_id,operation) DO NOTHING",
            params![operation, parent, detector.current_version()],
        )?;
    } else {
        db.execute(
            "INSERT INTO jobs(provider,username,operation,status,parent_id)
             VALUES ('archive','',?1,'queued',?2) ON CONFLICT(parent_id,operation) DO NOTHING",
            params![operation, parent],
        )?;
    }
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

pub fn archive_staged_record(
    db: &mut Connection,
    game: &Candidate,
    job: i64,
    offset: u64,
) -> Result<()> {
    archive_game(db, game, job, |tx, rejected| {
        job_store::staged_record(tx, job, offset, rejected)
    })
}

pub fn archive_upload(db: &mut Connection, game: &Candidate, job: i64, offset: u64) -> Result<()> {
    archive_game(db, game, job, |tx, rejected| {
        job_store::yield_turn(tx, job, Some(&offset.to_string()), usize::from(rejected))
    })
}

fn archive_game(
    db: &mut Connection,
    game: &Candidate,
    job: i64,
    checkpoint: impl FnOnce(&rusqlite::Transaction<'_>, bool) -> Result<()>,
) -> Result<()> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let (game_id, added) = match games::accept_game(&tx, game)? {
        Acceptance::Added(id) => (id, true),
        Acceptance::Existing(id) => (id, false),
        Acceptance::ConflictingOutcome => {
            checkpoint(&tx, true)?;
            tx.commit()?;
            return Ok(());
        }
    };
    if tx.execute(
        "INSERT OR IGNORE INTO job_games(job_id,game_id,scanned) VALUES (?1,?2,1)",
        params![job, game_id],
    )? == 1
    {
        tx.execute(
            "UPDATE jobs SET added=added+?1,existing=existing+?2 WHERE id=?3",
            params![added, !added, job],
        )?;
        if added {
            enqueue(&tx, job, Detector::Repertoire.job(false), &game_id)?;
            enqueue(&tx, job, Detector::Capture.job(false), &game_id)?;
        }
    }
    checkpoint(&tx, false)?;
    tx.commit()?;
    Ok(())
}

pub fn pending_game(db: &Connection, job: i64) -> Result<Option<String>> {
    Ok(db
        .query_row(
            "SELECT game_id FROM job_games WHERE job_id=?1 AND scanned=0 ORDER BY game_id LIMIT 1",
            [job],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn start_scan(db: &mut Connection, job: i64, cursor: &str) -> Result<()> {
    let tx = db.transaction_with_behavior(Immediate)?;
    for detector in [Detector::Repertoire, Detector::Capture] {
        let child = stage(&tx, job, detector.job(true))?;
        tx.execute(
            "UPDATE jobs SET cursor=?2 WHERE id=?1",
            params![child, cursor],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn prepare_capture(
    db: &Connection,
    game: &str,
    capture: CaptureProgram,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let snapshot = db.unchecked_transaction()?;
    let archived = games::read(&snapshot, game)?;
    let mut bytes = Vec::new();
    for decision in scanner::decisions(&archived.mainline, archived.learner)? {
        ensure!(Instant::now() < deadline, "Games turn timed out.");
        if let Some(finding) = capture(&decision) {
            let encoded = serde_json::to_vec(&finding)?;
            ensure!(
                bytes.len() + encoded.len() < 32 * 1024 * 1024,
                "detector output exceeds 32 MiB"
            );
            bytes.extend_from_slice(&encoded);
            bytes.push(b'\n');
        }
    }
    snapshot.commit()?;
    Ok(bytes)
}

pub fn seal_detector(db: &mut Connection, job: i64, game: &str, file: &str) -> Result<()> {
    let tx = db.transaction_with_behavior(Immediate)?;
    ensure!(
        tx.execute(
            "UPDATE jobs SET detector_game=?2,upload_file=?3,window_offset=0
             WHERE id=?1 AND status='running' AND upload_file IS NULL",
            params![job, game, file],
        )? == 1,
        "detector claim changed"
    );
    job_store::yield_turn(&tx, job, None, 0)?;
    tx.commit()?;
    Ok(())
}

// A new capture can write a finding, a qualification row and a counter update.
// Keep the remaining eight writes available for the scan checkpoint.
pub const MAX_FINDINGS_PER_APPLY: usize = 40;
pub const MAX_APPLY_ROWS: u64 = 128;

pub struct Mismatch<'a> {
    pub game: &'a str,
    pub ply: usize,
    pub position: &'a str,
    pub repertoire: &'a str,
    pub answer_version: &'a str,
    pub played: &'a str,
    pub detector_version: i64,
}

pub fn write_mismatch(db: &Connection, miss: Mismatch<'_>) -> Result<()> {
    db.execute(
        "INSERT OR IGNORE INTO mismatches VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            miss.game,
            miss.ply,
            miss.position,
            miss.repertoire,
            miss.answer_version,
            miss.played,
            miss.detector_version
        ],
    )?;
    Ok(())
}

pub struct DetectorBatch {
    pub job: i64,
    pub detector: Detector,
    pub version: i64,
    pub game: String,
    pub file: Option<String>,
    pub before: u64,
    pub after: u64,
    pub findings: Vec<Observation>,
    pub finished: bool,
    pub cursor: Option<String>,
}

pub fn apply_detector(db: &mut Connection, batch: &DetectorBatch) -> Result<()> {
    ensure!(
        batch.findings.len() <= MAX_FINDINGS_PER_APPLY,
        "scan batch exceeds row budget"
    );
    ensure!(
        batch.file.is_some() || (batch.finished && batch.findings.is_empty()),
        "only empty work can skip sealing"
    );
    let tx = db.transaction_with_behavior(Immediate)?;
    let before = tx.total_changes();
    let sealed_game = batch.file.as_ref().map(|_| &batch.game);
    ensure!(
        tx.execute(
            "UPDATE jobs SET window_offset=?6,
                upload_file=CASE WHEN ?7 THEN NULL ELSE upload_file END,
                detector_game=CASE WHEN ?7 THEN NULL ELSE detector_game END
             WHERE id=?1 AND detector_game IS ?2 AND upload_file IS ?3
               AND window_offset=?4 AND detector_version=?5 AND status='running' AND operation=?8",
            params![
                batch.job,
                sealed_game,
                batch.file,
                batch.before,
                batch.version,
                batch.after,
                batch.finished,
                batch.detector.job(batch.cursor.is_some()),
            ],
        )? == 1,
        "detector checkpoint changed"
    );
    let mut found = 0;
    for observation in &batch.findings {
        ensure!(
            batch.detector.accepts(observation),
            "unexpected detector observation"
        );
        match observation {
            Observation::Repertoire {
                ply,
                position,
                repertoire,
                answer_version,
                played,
            } => {
                write_mismatch(
                    &tx,
                    Mismatch {
                        game: &batch.game,
                        ply: *ply,
                        position,
                        repertoire,
                        answer_version,
                        played,
                        detector_version: batch.version,
                    },
                )?;
            }
            Observation::Capture {
                ply,
                position,
                fen,
                played,
                captures,
            } => {
                found += tx.execute(
                    "INSERT OR IGNORE INTO missed_captures(game_id,ply,position_id,played_move,captures,fen,detector_version)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params![batch.game,ply,position,played,serde_json::to_string(captures)?,fen,batch.version],
                )?;
            }
        }
    }
    if found != 0 {
        tx.execute(
            "UPDATE jobs SET found=found+?1 WHERE id=?2",
            params![found, batch.job],
        )?;
        if batch.cursor.is_some() {
            tx.execute(
                "UPDATE jobs SET found=found+?1 WHERE id=(SELECT parent_id FROM jobs WHERE id=?2)",
                params![found, batch.job],
            )?;
        }
    }
    if batch.finished {
        complete_scan_game(
            &tx,
            batch.job,
            &batch.game,
            batch.detector,
            batch.cursor.is_some(),
        )?;
    }
    let cursor = batch.cursor.as_deref().filter(|_| batch.finished);
    job_store::yield_turn(&tx, batch.job, cursor, 0)?;
    ensure!(
        tx.total_changes() - before <= MAX_APPLY_ROWS,
        "scan apply exceeds row budget"
    );
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

fn complete_scan_game(
    db: &Connection,
    job: i64,
    game: &str,
    detector: Detector,
    archive: bool,
) -> Result<()> {
    complete(db, job, game)?;
    if archive {
        let sibling = match detector {
            Detector::Repertoire => Detector::Capture,
            Detector::Capture => Detector::Repertoire,
        };
        db.execute(
            "UPDATE jobs SET added=added+1 WHERE id=(SELECT parent_id FROM jobs WHERE id=?1)
             AND EXISTS(SELECT 1 FROM jobs sibling JOIN job_games done ON done.job_id=sibling.id
               WHERE sibling.parent_id=jobs.id AND sibling.operation=?2 AND done.game_id=?3 AND done.scanned=1)",
            params![job, sibling.job(true), game],
        )?;
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
pub struct FeedbackCursor {
    pub game: String,
    after: String,
    waiting: Option<FeedbackWait>,
}

#[derive(Serialize, Deserialize)]
struct FeedbackWait {
    repertoire: String,
    generation: i64,
}

pub fn feedback_cursor(db: &Connection, job: i64) -> Result<Option<FeedbackCursor>> {
    let cursor: Option<String> = db.query_row(
        "SELECT feedback_cursor FROM jobs WHERE id=?1",
        [job],
        |row| row.get(0),
    )?;
    Ok(cursor.as_deref().map(serde_json::from_str).transpose()?)
}

fn save_feedback_cursor(db: &Connection, job: i64, cursor: Option<&FeedbackCursor>) -> Result<()> {
    db.execute(
        "UPDATE jobs SET feedback_cursor=?2 WHERE id=?1",
        params![job, cursor.map(serde_json::to_string).transpose()?],
    )?;
    Ok(())
}

pub fn retry_feedback_feeder(db: &Connection, job: i64) -> Result<()> {
    if let Some(mut cursor) = feedback_cursor(db, job)? {
        cursor.waiting = None;
        save_feedback_cursor(db, job, Some(&cursor))?;
    }
    Ok(())
}

/// One addressed feeder step. It shares the explicit comparison's durable
/// work without advancing past an accepted but unfinished comparison.
pub fn feed_feedback(
    db: &mut Connection,
    job: i64,
    game: &str,
    archive_next: Option<&str>,
) -> Result<()> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let mut cursor = feedback_cursor(&tx, job)?.unwrap_or_else(|| FeedbackCursor {
        game: game.to_owned(),
        after: String::new(),
        waiting: None,
    });
    ensure!(cursor.game == game, "comparison feeder game changed");
    if let Some(waiting) = &cursor.waiting {
        let pair = Pair {
            game: game.to_owned(),
            repertoire: waiting.repertoire.clone(),
        };
        if game_feedback::feed_settled(&tx, &pair, waiting.generation)? {
            cursor.after = pair.repertoire;
            cursor.waiting = None;
            save_feedback_cursor(&tx, job, Some(&cursor))?;
        }
    } else if let Some(repertoire) =
        catalog::next_repertoire(&tx, &games::learner_color(&tx, game)?, &cursor.after)?
    {
        let pair = Pair {
            game: game.to_owned(),
            repertoire,
        };
        match game_feedback::feed(&tx, &pair)? {
            Admission::Current => cursor.after = pair.repertoire,
            Admission::Wait(generation) => {
                cursor.waiting = Some(FeedbackWait {
                    repertoire: pair.repertoire,
                    generation,
                })
            }
            Admission::Full => (),
        }
        save_feedback_cursor(&tx, job, Some(&cursor))?;
    } else {
        complete_scan_game(&tx, job, game, Detector::Repertoire, archive_next.is_some())?;
        save_feedback_cursor(&tx, job, None)?;
        job_store::yield_turn(&tx, job, archive_next, 0)?;
        tx.commit()?;
        return Ok(());
    }
    job_store::yield_turn(&tx, job, None, 0)?;
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
    while let Some(row) = rows.next()? {
        let game: String = row.get(0)?;
        let ply: usize = row.get(1)?;
        let position: String = row.get(2)?;
        let board = chess::board_at(&games::legacy_route(db, &game)?, ply)?;
        ensure!(
            chess::position_id(&board) == position,
            "legacy missed capture position changed"
        );
        db.execute(
            "UPDATE missed_captures SET fen=?3 WHERE game_id=?1 AND ply=?2",
            params![game, ply, chess::fen(&board)],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initialize_fixture(db: &Connection) -> Result<()> {
        db.execute_batch(include_str!("schema.sql"))?;
        db.execute_batch(include_str!("opening_policy.sql"))?;
        db.execute_batch(include_str!("game_pipeline.sql"))?;
        db.execute_batch(include_str!("opening_order.sql"))?;
        db.execute_batch(include_str!("prepared_queue.sql"))?;
        db.execute_batch(include_str!("lichess_packs.sql"))?;
        db.execute_batch(include_str!("job_turns.sql"))?;
        db.execute_batch(include_str!("operation_bounds.sql"))?;
        games::upgrade(db)?;
        db.execute_batch(include_str!("detectors.sql"))?;
        db.execute_batch(include_str!("card_availability.sql"))?;
        db.execute_batch(include_str!("tactic_qualification.sql"))?;
        db.execute_batch(include_str!("capture_status.sql"))?;
        db.execute("INSERT INTO metadata VALUES ('fixture')", [])?;
        Ok(())
    }

    #[test]
    fn final_capture_turn_counts_index_trigger_and_progress_writes_in_the_budget() -> Result<()> {
        let db = Connection::open_in_memory()?;
        initialize_fixture(&db)?;
        let mut db = db;
        let game = games::archive_game(
            chess::parse_pgn(
                "[White \"Learner\"]\n[Black \"Opponent\"]\n\n1. e4 e5 1-0",
                false,
                1,
            )?
            .remove(0)?,
            "Learner",
        )?;
        let Acceptance::Added(game) = games::accept_game(&db, &game)? else {
            panic!("new fixture game")
        };
        db.execute("INSERT INTO jobs(provider,username,operation,status) VALUES ('archive','','scan','running')", [])?;
        let parent = db.last_insert_rowid();
        start_scan(&mut db, parent, "fixture-cursor")?;
        let comparison = stage(&db, parent, Detector::Repertoire.job(true))?;
        let capture = stage(&db, parent, Detector::Capture.job(true))?;
        complete(&db, comparison, &game)?;
        db.execute("UPDATE jobs SET status='running' WHERE id=?1", [capture])?;
        seal_detector(&mut db, capture, &game, "fixture")?;
        db.execute("UPDATE jobs SET status='running' WHERE id=?1", [capture])?;
        let findings = (0..MAX_FINDINGS_PER_APPLY)
            .map(|ply| Observation::Capture {
                ply,
                position: "fixture".into(),
                fen: "fixture".into(),
                played: "e2e4".into(),
                captures: vec!["d1d5".into()],
            })
            .collect();
        let before = db.total_changes();
        apply_detector(
            &mut db,
            &DetectorBatch {
                job: capture,
                detector: Detector::Capture,
                version: 1,
                game,
                file: Some("fixture".into()),
                before: 0,
                after: 1,
                findings,
                finished: true,
                cursor: Some("advanced".into()),
            },
        )?;
        let changes = db.total_changes() - before;
        assert!(changes > 2 * MAX_FINDINGS_PER_APPLY as u64 && changes <= MAX_APPLY_ROWS);
        Ok(())
    }

    #[test]
    fn retry_publication_uses_saved_capture_and_leaves_archive_and_scan_complete() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        initialize_fixture(&db)?;
        db.execute("INSERT INTO jobs(provider,username,operation,status) VALUES ('pgn','Learner','upload','done')", [])?;
        let text = "[White \"Learner\"]\n[Black \"Opponent\"]\n[Result \"1/2-1/2\"]\n[SetUp \"1\"]\n[FEN \"4k3/8/8/n2n4/8/8/8/R2RK3 w - - 0 1\"]\n\n1. Ke2 1/2-1/2";
        let game = games::archive_game(chess::parse_pgn(text, false, 1)?.remove(0)?, "Learner")?;
        archive_upload(&mut db, &game, 1, 10)?;
        archive_upload(&mut db, &game, 1, 20)?;
        let scan = stage(&db, 1, Detector::Capture.job(false))?;
        assert_eq!(
            pending_game(&db, scan)?.as_deref(),
            Some(game.game.id.as_str())
        );
        let bytes = prepare_capture(
            &db,
            &game.game.id,
            scanner::capture_program(1)?,
            Instant::now() + std::time::Duration::from_secs(5),
        )?;
        let findings = serde_json::Deserializer::from_slice(&bytes)
            .into_iter::<Observation>()
            .collect::<std::result::Result<Vec<_>, _>>()?;
        db.execute("UPDATE jobs SET status='running' WHERE id=?1", [scan])?;
        seal_detector(&mut db, scan, &game.game.id, "fixture")?;
        db.execute("UPDATE jobs SET status='running' WHERE id=?1", [scan])?;
        let batch = DetectorBatch {
            job: scan,
            detector: Detector::Capture,
            version: 1,
            game: game.game.id.clone(),
            file: Some("fixture".into()),
            before: 0,
            after: bytes.len() as u64,
            findings,
            finished: true,
            cursor: None,
        };
        apply_detector(&mut db, &batch)?;
        assert!(apply_detector(&mut db, &batch).is_err());
        assert!(pending_game(&db, scan)?.is_none());
        assert_eq!(
            db.query_row("SELECT count(*) FROM missed_captures", [], |r| r
                .get::<_, usize>(0))?,
            1
        );
        assert!(catalog::tactics(&db, "")?.is_empty());
        crate::capture_publication::test_support::publish_fixture(&mut db)?;
        let original_tactic: (String, String) =
            db.query_row("SELECT id, route FROM tactics", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
        assert_eq!(catalog::tactics(&db, "")?.len(), 1);

        // Substitute a detector using the same addressed game and persisted
        // apply path. This version keeps a single candidate from the result.
        let program: CaptureProgram = |decision| {
            let mut finding = scanner::capture(decision)?;
            if let Observation::Capture { captures, .. } = &mut finding {
                captures.truncate(1);
            }
            Some(finding)
        };
        let replacement = prepare_capture(
            &db,
            &game.game.id,
            program,
            Instant::now() + std::time::Duration::from_secs(5),
        )?;
        let findings = serde_json::Deserializer::from_slice(&replacement)
            .into_iter::<Observation>()
            .collect::<std::result::Result<Vec<_>, _>>()?;
        db.execute("INSERT INTO jobs(provider,username,operation,status) VALUES ('archive','','scan','done')", [])?;
        let parent = db.last_insert_rowid();
        let second = stage(&db, parent, Detector::Capture.job(false))?;
        db.execute(
            "UPDATE jobs SET status='running',detector_version=2 WHERE id=?1",
            [second],
        )?;
        seal_detector(&mut db, second, &game.game.id, "replacement")?;
        db.execute("UPDATE jobs SET status='running' WHERE id=?1", [second])?;
        apply_detector(
            &mut db,
            &DetectorBatch {
                job: second,
                detector: Detector::Capture,
                version: 2,
                game: game.game.id,
                file: Some("replacement".into()),
                before: 0,
                after: replacement.len() as u64,
                finished: true,
                findings,
                cursor: None,
            },
        )?;
        let mut versions = db.prepare(
            "SELECT detector_version,json_array_length(captures) FROM missed_captures ORDER BY detector_version",
        )?;
        let lengths = versions
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(lengths, [(1, 2), (2, 1)]);
        assert_eq!(
            db.query_row("SELECT id,route FROM tactics", [], |row| Ok((
                row.get(0)?,
                row.get(1)?
            )))?,
            original_tactic,
        );
        assert!(!db.query_row(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE parent_id=?1 AND operation='publish')",
            [second],
            |row| row.get::<_, bool>(0)
        )?);
        Ok(())
    }
}
