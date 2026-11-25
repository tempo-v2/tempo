use crate::{
    catalog,
    db::Database,
    game_pipeline,
    jobs::{Job, JobView, NewJob, valid_user},
    providers::Provider,
    scanner::Program,
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::path::Path;
use std::time::{Duration, Instant};

pub fn settings(db: &Connection) -> Result<std::collections::BTreeMap<String, String>> {
    let mut q = db.prepare("SELECT provider,username FROM settings")?;
    let rows = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn save_settings(
    db: &mut Connection,
    names: std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let tx = db.transaction()?;
    for (provider, name) in names {
        ensure!(Provider::named(&provider).is_some(), "unknown provider");
        if name.trim().is_empty() {
            tx.execute("DELETE FROM settings WHERE provider=?1", [&provider])?;
        } else {
            tx.execute("INSERT INTO settings VALUES (?1,?2) ON CONFLICT(provider) DO UPDATE SET username=excluded.username",
                params![provider,valid_user(&name)?])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn views(db: &Connection) -> Result<Vec<JobView>> {
    let mut q = db.prepare(
        "WITH visible(id) AS (
            SELECT id FROM (
                SELECT id FROM jobs INDEXED BY jobs_visible_latest
                WHERE operation NOT IN ('compare_all','capture_all') ORDER BY id DESC LIMIT 12
            ) UNION SELECT id FROM (
                SELECT id FROM jobs INDEXED BY jobs_visible_failed
                WHERE status='failed' AND operation NOT IN ('compare_all','capture_all')
                ORDER BY id DESC LIMIT 12
            ) UNION SELECT id FROM (
                SELECT id FROM jobs INDEXED BY jobs_active_user
                WHERE parent_id IS NULL AND operation!='publish' AND status IN ('queued','running')
                LIMIT 4
            )
         )
         SELECT j.id,j.provider,j.operation,j.status,j.added,j.existing,j.rejected,j.found,j.error
         FROM visible v JOIN jobs j ON j.id=v.id ORDER BY j.id DESC",
    )?;
    let rows = q.query_map([], |r| {
        Ok(JobView {
            id: r.get(0)?,
            provider: r.get(1)?,
            operation: r.get(2)?,
            status: r.get(3)?,
            added: r.get(4)?,
            existing: r.get(5)?,
            rejected: r.get(6)?,
            found: r.get(7)?,
            error: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn check_user_job_capacity(db: &Connection) -> Result<()> {
    let full: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM jobs INDEXED BY jobs_active_user
         WHERE parent_id IS NULL AND operation!='publish' AND status IN ('queued','running')
         LIMIT 1 OFFSET 3)",
        [],
        |r| r.get(0),
    )?;
    ensure!(!full, "the import queue is full");
    Ok(())
}

fn active_user_job(
    db: &Connection,
    provider: &str,
    operation: &str,
    username: &str,
) -> Result<Option<i64>> {
    Ok(db
        .query_row(
            "SELECT id FROM jobs INDEXED BY jobs_active_user
             WHERE parent_id IS NULL AND operation!='publish' AND status IN ('queued','running')
               AND provider=?1 AND operation=?2 AND username=?3
             ORDER BY status,id LIMIT 1",
            params![provider, operation, username],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn start(db: &mut Connection, input: NewJob) -> Result<i64> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    check_user_job_capacity(&tx)?;
    let (provider, operation, username, upload) = match input {
        NewJob::Command(input) => {
            let username = match (input.provider.as_str(), input.operation.as_str()) {
                ("archive", "scan") => String::new(),
                (name, "all" | "new") if Provider::named(name).is_some() => tx
                    .query_row(
                        "SELECT username FROM settings WHERE provider=?1",
                        [&input.provider],
                        |r| r.get(0),
                    )
                    .optional()?
                    .context("save the username in Settings first")?,
                _ => bail!("unknown import operation"),
            };
            if let Some(job) = active_user_job(&tx, &input.provider, &input.operation, &username)? {
                return Ok(job);
            }
            (input.provider, input.operation, username, None)
        }
        NewJob::Upload { username, file } => ("pgn".into(), "upload".into(), username, Some(file)),
    };
    tx.execute(
        "INSERT INTO jobs(provider,username,operation,upload_file,status) VALUES (?1,?2,?3,?4,'queued')",
        params![provider, username, operation, upload],
    )?;
    let id = tx.last_insert_rowid();
    tx.commit()?;
    Ok(id)
}

pub fn retry(db: &mut Connection, id: i64) -> Result<()> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let user_batch = tx
        .query_row(
            "SELECT parent_id IS NULL AND operation!='publish' FROM jobs WHERE id=?1",
            [id],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false);
    if user_batch {
        check_user_job_capacity(&tx)?;
    }
    let lanes = tx.execute(
        "UPDATE jobs SET status='queued',error=NULL
         WHERE parent_id=?1 AND operation IN ('compare_all','capture_all') AND status='failed'
           AND EXISTS(SELECT 1 FROM jobs p WHERE p.id=?1 AND p.status='failed')",
        [id],
    )?;
    if lanes != 0 {
        tx.execute(
            "UPDATE jobs SET status='running',error=NULL WHERE id=?1",
            [id],
        )?;
    } else {
        ensure!(
            tx.execute(
                "UPDATE jobs SET status='queued',error=NULL WHERE id=?1 AND status='failed'
                 AND operation NOT IN ('compare_all','capture_all')",
                [id],
            )? == 1,
            "the job is not waiting for retry"
        );
    }
    tx.commit()?;
    Ok(())
}

/// The public archive scan remains running while its two child lanes rotate.
fn refresh_scan_parent(db: &Connection, child: i64) -> Result<()> {
    db.execute(
        "UPDATE jobs SET
         status=CASE
           WHEN EXISTS(SELECT 1 FROM jobs lane WHERE lane.parent_id=jobs.id
                        AND lane.operation IN ('compare_all','capture_all') AND lane.status='failed') THEN 'failed'
           WHEN NOT EXISTS(SELECT 1 FROM jobs lane WHERE lane.parent_id=jobs.id
                            AND lane.operation IN ('compare_all','capture_all') AND lane.status!='done') THEN 'done'
           ELSE 'running' END,
         error=(SELECT lane.error FROM jobs lane WHERE lane.parent_id=jobs.id
                AND lane.operation IN ('compare_all','capture_all') AND lane.status='failed'
                ORDER BY lane.id LIMIT 1)
         WHERE id=(SELECT parent_id FROM jobs WHERE id=?1 AND operation IN ('compare_all','capture_all'))",
        [child],
    )?;
    Ok(())
}

#[derive(Clone)]
pub struct FailedTurn {
    pub job_id: i64,
    pub message: String,
}

/// Record a failed turn with the next claim. A refused admission leaves both untouched.
pub fn next(db: &mut Connection, failed: Option<&FailedTurn>) -> Result<Option<Job>> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(failed) = failed {
        tx.execute(
            "UPDATE jobs SET status='failed',error=?2 WHERE id=?1 AND status='running'",
            params![failed.job_id, failed.message],
        )?;
        refresh_scan_parent(&tx, failed.job_id)?;
    }
    let job = tx.query_row("SELECT id,provider,username,operation,window_offset,window_rejected,replay_upload FROM jobs INDEXED BY jobs_ready_order WHERE status='queued' ORDER BY ready_order,id LIMIT 1",[],
        |r| Ok(Job { id:r.get(0)?,provider:r.get(1)?,username:r.get(2)?,operation:r.get(3)?,window_offset:r.get(4)?,window_rejected:r.get(5)?,replay_upload:r.get(6)? })).optional()?;
    if let Some(job) = &job {
        tx.execute("UPDATE jobs SET status='running' WHERE id=?1", [job.id])?;
    }
    tx.commit()?;
    Ok(job)
}

pub fn recover_running(db: &mut Connection) -> Result<usize> {
    Ok(db.execute(
        "UPDATE jobs SET status='queued' WHERE id IN
        (SELECT id FROM jobs WHERE status='running' AND NOT (
            provider='archive' AND operation='scan' AND EXISTS(
                SELECT 1 FROM jobs lane WHERE lane.parent_id=jobs.id AND lane.operation='compare_all'
            )) ORDER BY id LIMIT 128)",
        [],
    )?)
}

pub fn complete(
    db: &mut Connection,
    id: i64,
    provider: &str,
    username: &str,
    checkpoint: Option<(i64, usize)>,
) -> Result<()> {
    let tx = db.transaction()?;
    if let Some((through, _)) = checkpoint {
        tx.execute("INSERT INTO provider_checkpoint VALUES (?1,?2,?3)
            ON CONFLICT(provider,username) DO UPDATE SET through_at=max(through_at,excluded.through_at)",
            params![provider,username,through])?;
    }
    tx.execute(
        "UPDATE jobs SET status='done',payload=NULL,upload_file=NULL,error=NULL,
          rejected=CASE WHEN replay_upload=1 THEN window_rejected ELSE max(rejected,?2) END,
          replay_upload=0,window_offset=0,window_rejected=0 WHERE id=?1",
        params![id, checkpoint.map_or(0, |(_, rejected)| rejected)],
    )?;
    refresh_scan_parent(&tx, id)?;
    tx.commit()?;
    Ok(())
}

/// Commit the completed turn and return its job to the tail in the same transaction.
pub fn yield_turn(
    tx: &Transaction<'_>,
    id: i64,
    cursor: Option<&str>,
    rejected: usize,
) -> Result<()> {
    let moved = tx.execute(
        "UPDATE jobs SET status='queued',cursor=coalesce(?2,cursor),rejected=rejected+?3,
           ready_order=(SELECT last_order+1 FROM job_ready_clock WHERE singleton=1)
         WHERE id=?1 AND status='running'",
        params![id, cursor, rejected],
    )?;
    if moved != 0 {
        tx.execute(
            "UPDATE job_ready_clock SET last_order=last_order+1 WHERE singleton=1",
            [],
        )?;
    }
    Ok(())
}

/// Rotate after acquiring or finishing a provider window. The file and cursor change together.
pub fn checkpoint_provider_window(
    db: &mut Connection,
    id: i64,
    cursor: &str,
    upload: Option<&str>,
    rejected: usize,
) -> Result<()> {
    let tx = db.transaction()?;
    ensure!(
        tx.execute(
            "UPDATE jobs SET upload_file=?2,window_offset=0,window_rejected=0
             WHERE id=?1 AND status='running'",
            params![id, upload],
        )? == 1,
        "provider window is no longer active"
    );
    yield_turn(&tx, id, Some(cursor), rejected)?;
    tx.commit()?;
    Ok(())
}

pub fn staged_record(tx: &Transaction<'_>, id: i64, next: u64, rejected: bool) -> Result<()> {
    ensure!(
        tx.execute(
            "UPDATE jobs SET window_offset=?2,window_rejected=window_rejected+?3
             WHERE id=?1 AND status='running' AND window_offset<?2",
            params![id, next, rejected],
        )? == 1,
        "staged byte checkpoint did not advance"
    );
    yield_turn(tx, id, None, 0)
}

pub fn reject_staged_record(db: &mut Connection, id: i64, next: u64) -> Result<()> {
    let tx = db.transaction()?;
    staged_record(&tx, id, next, true)?;
    tx.commit()?;
    Ok(())
}

/// The staged bytes are durable before replacing the old inline representation.
pub fn stage_inline_upload(db: &mut Connection, id: i64, file: &str) -> Result<()> {
    let tx = db.transaction()?;
    ensure!(
        tx.execute(
            "UPDATE jobs SET upload_file=?2,payload=NULL
             WHERE id=?1 AND status='running' AND upload_file IS NULL AND payload IS NOT NULL",
            params![id, file],
        )? == 1,
        "the inline upload is no longer active"
    );
    yield_turn(&tx, id, None, 0)?;
    tx.commit()?;
    Ok(())
}

pub const ARCHIVE_RESUME_PAGE: u8 = 64;

pub struct ArchiveGame {
    pub ordinal: i64,
    pub id: String,
}

pub struct JobReads(Connection);
impl JobReads {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self(Database::read_with_busy_timeout(
            path,
            Duration::ZERO,
        )?))
    }

    pub fn within_turn<T>(&self, deadline: Instant, work: impl FnOnce() -> Result<T>) -> Result<T> {
        self.0.busy_timeout(Duration::ZERO)?;
        self.0
            .progress_handler(1_000, Some(move || Instant::now() >= deadline));
        let result = work();
        self.0.progress_handler(0, None::<fn() -> bool>);
        result
    }

    pub fn cursor(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .0
            .query_row("SELECT cursor FROM jobs WHERE id=?1", [id], |row| {
                row.get(0)
            })?)
    }

    pub fn upload(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .0
            .query_row("SELECT upload_file FROM jobs WHERE id=?1", [id], |row| {
                row.get(0)
            })?)
    }

    pub fn inline_upload(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .0
            .query_row("SELECT payload FROM jobs WHERE id=?1", [id], |row| {
                row.get(0)
            })?)
    }

    pub fn checkpoint(&self, provider: &str, user: &str) -> Result<Option<i64>> {
        Ok(self
            .0
            .query_row(
                "SELECT through_at FROM provider_checkpoint WHERE provider=?1 AND username=?2",
                params![provider, user],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn archive_end(&self) -> Result<i64> {
        Ok(self
            .0
            .query_row("SELECT coalesce(max(ordinal),0) FROM games", [], |row| {
                row.get(0)
            })?)
    }

    pub fn archive_page(&self, after: i64, through: i64, limit: u8) -> Result<Vec<ArchiveGame>> {
        ensure!(
            (1..=ARCHIVE_RESUME_PAGE).contains(&limit),
            "invalid archive page size"
        );
        let mut query = self.0.prepare_cached(
            "SELECT ordinal,id FROM games
             WHERE ordinal>?1 AND ordinal<=?2 ORDER BY ordinal LIMIT ?3",
        )?;
        let rows = query.query_map(params![after, through, limit], |row| {
            Ok(ArchiveGame {
                ordinal: row.get(0)?,
                id: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn archive_ordinal(&self, id: &str) -> Result<i64> {
        Ok(self
            .0
            .query_row("SELECT ordinal FROM games WHERE id=?1", [id], |row| {
                row.get(0)
            })?)
    }

    pub fn detector_plan(
        &self,
        game: &str,
        detector: Program,
        deadline: Instant,
    ) -> Result<Vec<u8>> {
        game_pipeline::prepare_detector(&self.0, game, detector, deadline)
    }

    pub fn detector_checkpoint(&self, job: i64) -> Result<(i64, Option<(String, String)>)> {
        let (version, game, file): (i64, Option<String>, Option<String>) = self.0.query_row(
            "SELECT detector_version,detector_game,upload_file FROM jobs WHERE id=?1",
            [job],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        ensure!(
            game.is_some() == file.is_some(),
            "incomplete sealed detector checkpoint"
        );
        Ok((version, game.zip(file)))
    }

    pub fn pending_game(&self, job: i64) -> Result<Option<String>> {
        game_pipeline::pending_game(&self.0, job)
    }

    pub fn publication_batch(&self, game: &str) -> Result<Vec<catalog::TacticPlan>> {
        game_pipeline::publication_batch(&self.0, game)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_page_work_is_independent_of_games_past_the_cutoff() -> Result<()> {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::Relaxed},
        };

        let db = Connection::open_in_memory()?;
        db.execute_batch(include_str!("game_ir.sql"))?;
        db.execute_batch(
            "ALTER TABLE games_v7 RENAME TO games;
             WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<65)
             INSERT INTO games(id,white,black,learner_color,game_ir,occurrence,outcome)
             SELECT printf('z%06d',i),'w','b','white',zeroblob(8),'[]','*' FROM n",
        )?;
        let db = JobReads(db);
        let operations = [(0, 65, 1), (65, 65, 1), (0, 65, ARCHIVE_RESUME_PAGE)];
        let mut baseline = Vec::new();
        for with_new_games in [false, true] {
            if with_new_games {
                db.0.execute_batch(
                    "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<100000)
                     INSERT INTO games(id,white,black,learner_color,game_ir,occurrence,outcome)
                     SELECT printf('a%06d',i),'w','b','white',zeroblob(8),'[]','*' FROM n",
                )?;
            }
            let steps = Arc::new(AtomicUsize::new(0));
            let count = steps.clone();
            db.0.progress_handler(
                1,
                Some(move || {
                    count.fetch_add(1, Relaxed);
                    false
                }),
            );
            for (index, (after, through, limit)) in operations.into_iter().enumerate() {
                let page = db.archive_page(after, through, limit)?;
                let work = steps.swap(0, Relaxed);
                let actual: Vec<_> = page
                    .into_iter()
                    .map(|game| (game.ordinal, game.id))
                    .collect();
                let expected: Vec<_> = ((after + 1)..=through)
                    .take(usize::from(limit))
                    .map(|ordinal| (ordinal, format!("z{ordinal:06}")))
                    .collect();
                assert_eq!(actual, expected);
                assert!(work <= if limit == 1 { 128 } else { 1_000 }, "{work} steps");
                if with_new_games {
                    assert!(
                        work <= baseline[index] + 8,
                        "VM work grew from {} to {work}",
                        baseline[index]
                    );
                } else {
                    baseline.push(work);
                }
                eprintln!(
                    "archive page after {after}, limit {limit}, new games {with_new_games}: {work} steps"
                );
            }
            db.0.progress_handler(0, None::<fn() -> bool>);
        }
        let page = db.archive_page(65, 66, 1)?;
        assert_eq!(page.len(), 1);
        let game = &page[0];
        assert_eq!((game.ordinal, game.id.as_str()), (66, "a000001"));
        Ok(())
    }

    #[test]
    fn queued_turns_migrate_and_rotate_in_durable_order() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        for schema in [
            include_str!("schema.sql"),
            include_str!("opening_policy.sql"),
            include_str!("opening_order.sql"),
            include_str!("game_pipeline.sql"),
            include_str!("prepared_queue.sql"),
            include_str!("lichess_packs.sql"),
        ] {
            db.execute_batch(schema)?;
        }
        db.execute_batch(
            "INSERT INTO jobs(provider,username,operation,status) VALUES
             ('archive','','scan','running'), ('archive','','scan','queued'),
             ('archive','','scan','queued')",
        )?;
        db.execute_batch(include_str!("job_turns.sql"))?;
        db.execute_batch(include_str!("operation_bounds.sql"))?;
        db.execute_batch(include_str!("job_admission.sql"))?;
        db.execute_batch(include_str!("detectors.sql"))?;
        assert_eq!(recover_running(&mut db)?, 1);

        assert_eq!(next(&mut db, None)?.unwrap().id, 1);
        let tx = db.transaction()?;
        yield_turn(&tx, 1, Some("checkpoint"), 1)?;
        tx.commit()?;
        assert_eq!(next(&mut db, None)?.unwrap().id, 2);
        let failed = FailedTurn {
            job_id: 2,
            message: "fixture".into(),
        };
        assert_eq!(next(&mut db, Some(&failed))?.unwrap().id, 3);
        retry(&mut db, 2)?;
        complete(&mut db, 3, "archive", "", None)?;

        let resumed = next(&mut db, None)?.unwrap();
        assert_eq!(resumed.id, 1);
        assert_eq!(
            db.query_row("SELECT cursor FROM jobs WHERE id=?1", [resumed.id], |row| {
                row.get::<_, String>(0)
            })?,
            "checkpoint"
        );
        assert_eq!(
            views(&db)?.iter().find(|job| job.id == 1).unwrap().rejected,
            1
        );
        assert_eq!(next(&mut db, None)?.unwrap().id, 2);
        Ok(())
    }

    #[test]
    fn admission_work_excludes_children_and_finished_jobs() -> Result<()> {
        use crate::jobs::StartJob;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::Relaxed},
        };

        let mut db = Connection::open_in_memory()?;
        for schema in [
            include_str!("schema.sql"),
            include_str!("opening_policy.sql"),
            include_str!("opening_order.sql"),
            include_str!("game_pipeline.sql"),
            include_str!("prepared_queue.sql"),
            include_str!("lichess_packs.sql"),
            include_str!("job_turns.sql"),
            include_str!("operation_bounds.sql"),
            include_str!("job_admission.sql"),
        ] {
            db.execute_batch(schema)?;
        }
        let scan = || {
            NewJob::Command(StartJob {
                provider: "archive".into(),
                operation: "scan".into(),
            })
        };
        let first = start(&mut db, scan())?;
        let measure = |db: &Connection| -> Result<[usize; 3]> {
            let steps = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&steps);
            db.progress_handler(
                1,
                Some(move || {
                    counter.fetch_add(1, Relaxed);
                    false
                }),
            );
            check_user_job_capacity(db)?;
            let capacity = steps.swap(0, Relaxed);
            assert_eq!(active_user_job(db, "archive", "scan", "")?, Some(first));
            let hit = steps.swap(0, Relaxed);
            assert_eq!(active_user_job(db, "lichess", "new", "Fixture")?, None);
            let miss = steps.load(Relaxed);
            db.progress_handler(0, None::<fn() -> bool>);
            Ok([capacity, hit, miss])
        };
        let small = measure(&db)?;
        db.execute_batch(
            "WITH RECURSIVE n(i) AS (VALUES(2) UNION ALL SELECT i+1 FROM n WHERE i<100001)
             INSERT INTO jobs(id,parent_id,provider,username,operation,status)
             SELECT i,CASE WHEN i%2=0 THEN i END,'archive','','scan',
                    CASE WHEN i%2=0 THEN 'queued' ELSE 'done' END FROM n;",
        )?;
        let large = measure(&db)?;
        eprintln!("Games admission SQL steps [capacity, hit, miss]: {small:?} -> {large:?}");
        assert!(small.into_iter().chain(large).all(|steps| steps <= 100));
        assert_eq!(start(&mut db, scan())?, first);

        db.execute_batch(
            "INSERT INTO jobs(provider,username,operation,status) VALUES
            ('pgn','Fixture','upload','queued'), ('pgn','Fixture','upload','queued'),
            ('pgn','Fixture','upload','queued'), ('pgn','Fixture','upload','failed');",
        )?;
        let failed = db.last_insert_rowid();
        assert_eq!(
            start(&mut db, scan()).unwrap_err().to_string(),
            "the import queue is full"
        );
        assert_eq!(
            retry(&mut db, failed).unwrap_err().to_string(),
            "the import queue is full"
        );
        complete(&mut db, first, "archive", "", None)?;
        retry(&mut db, failed)?;
        Ok(())
    }

    #[test]
    fn recent_job_remains_visible_alongside_prior_failures() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(include_str!("schema.sql"))?;
        db.execute_batch(include_str!("opening_policy.sql"))?;
        db.execute_batch(include_str!("game_pipeline.sql"))?;
        db.execute_batch(include_str!("job_turns.sql"))?;
        db.execute_batch(include_str!("opening_order.sql"))?;
        db.execute_batch(include_str!("prepared_queue.sql"))?;
        db.execute_batch(include_str!("lichess_packs.sql"))?;
        db.execute_batch(include_str!("operation_bounds.sql"))?;
        db.execute_batch(include_str!("job_admission.sql"))?;
        db.execute_batch(include_str!("detectors.sql"))?;
        for _ in 0..12 {
            db.execute("INSERT INTO jobs(provider,username,operation,status) VALUES ('pgn','Learner','upload','failed')", [])?;
        }
        db.execute("INSERT INTO jobs(provider,username,operation,status) VALUES ('archive','','scan','queued')", [])?;
        let jobs = views(&db)?;
        assert_eq!(jobs[0].status, "queued");
        assert_eq!(jobs.iter().filter(|job| job.status == "failed").count(), 12);
        Ok(())
    }
}
