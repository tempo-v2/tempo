use anyhow::{Result, ensure};
use rusqlite::{Connection, OpenFlags};
use std::{path::Path, time::Duration};

const SCHEMA: i32 = 19;

pub struct Database;

impl Database {
    pub fn open(path: &Path) -> Result<Connection> {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        Self::configure(&db)?;
        ensure!(
            db.pragma_query_value(None, "application_id", |r| r.get::<_, i32>(0))? == 0x544d5032,
            "this is not a lean Tempo database"
        );
        Ok(db)
    }

    pub fn read(path: &Path) -> Result<Connection> {
        Self::read_with_busy_timeout(path, Duration::from_secs(2))
    }

    pub fn read_with_busy_timeout(path: &Path, busy_timeout: Duration) -> Result<Connection> {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        db.busy_timeout(busy_timeout)?;
        Self::configure_pragmas(&db)?;
        Ok(db)
    }

    pub fn init(path: &Path) -> Result<Connection> {
        if path.exists() {
            let db = Self::open(path)?;
            let version = Self::version(&db)?;
            ensure!(
                version <= SCHEMA,
                "this Tempo database requires a newer release"
            );
            ensure!(
                version == SCHEMA,
                "stop Tempo and run tempo migrate-games DATABASE before serving this database"
            );
            return Ok(db);
        }
        let db = Connection::open(path)?;
        Self::configure(&db)?;
        db.execute_batch("PRAGMA journal_mode=WAL; BEGIN IMMEDIATE;")?;
        db.execute_batch(include_str!("schema.sql"))?;
        db.execute_batch(include_str!("opening_policy.sql"))?;
        db.execute_batch(include_str!("opening_order.sql"))?;
        db.execute_batch(include_str!("game_pipeline.sql"))?;
        db.execute_batch(include_str!("prepared_queue.sql"))?;
        db.execute_batch(include_str!("job_turns.sql"))?;
        db.execute_batch(include_str!("operation_bounds.sql"))?;
        db.execute_batch(include_str!("job_admission.sql"))?;
        db.execute_batch(include_str!("coverage.sql"))?;
        db.execute_batch(include_str!("coverage_order.sql"))?;
        db.execute_batch(include_str!("gap_batches.sql"))?;
        db.execute_batch(include_str!("lichess_packs.sql"))?;
        db.execute_batch(include_str!("lichess_start_revision.sql"))?;
        crate::games::upgrade(&db)?;
        db.execute_batch(include_str!("detectors.sql"))?;
        db.execute_batch(include_str!("card_availability.sql"))?;
        db.execute_batch(include_str!("tactic_qualification.sql"))?;
        db.execute_batch(include_str!("opening_replacement.sql"))?;
        upgrade_views(&db)?;
        db.execute_batch(include_str!("rank_apply.sql"))?;
        crate::catalog::migrate_opening_identities(&db)?;
        db.execute_batch(include_str!("game_feedback.sql"))?;
        db.execute(
            "INSERT INTO metadata VALUES (?1)",
            [format!("{:032x}", rand::random::<u128>())],
        )?;
        db.execute_batch("PRAGMA application_id=1414352946;")?;
        db.pragma_update(None, "user_version", SCHEMA)?;
        db.execute_batch("COMMIT;")?;
        Ok(db)
    }

    fn version(db: &Connection) -> Result<i32> {
        Ok(db.pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    /// The caller holds the same exclusive file lock as the running service.
    pub fn migrate_games(path: &Path) -> Result<()> {
        let mut db = Self::open(path)?;
        let version = Self::version(&db)?;
        ensure!(
            version <= SCHEMA,
            "this Tempo database requires a newer release"
        );
        if version == SCHEMA {
            return Ok(());
        }
        if version >= 7 {
            // The ordinal cursor changes job interpretation, not the table layout.
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if version < 9 {
                tx.execute_batch(include_str!("coverage.sql"))?;
            }
            if version < 10 {
                tx.execute_batch(include_str!("coverage_order.sql"))?;
            }
            if version < 11 {
                tx.execute_batch(include_str!("gap_batches.sql"))?;
            }
            if version < 12 {
                tx.execute_batch(include_str!("lichess_packs.sql"))?;
            }
            if version < 13 {
                tx.execute_batch(include_str!("lichess_start_revision.sql"))?;
            }
            if version < 14 {
                tx.execute_batch(include_str!("card_availability.sql"))?;
                tx.execute_batch(include_str!("tactic_qualification.sql"))?;
            }
            if version < 16 {
                tx.execute_batch(include_str!("opening_replacement.sql"))?;
            }
            if version < 15 {
                upgrade_views(&tx)?;
            } else if version == 15 {
                tx.execute_batch(
                    "
                    ALTER TABLE coverage_view ADD COLUMN model_id TEXT NOT NULL DEFAULT 'null';
                    UPDATE coverage_view SET model_id=(
                        SELECT json_quote(coalesce(json_extract(report,'$.model'), 'Maia2'))
                        FROM coverage_report WHERE id=report_id
                    );
                ",
                )?;
            }
            if version < 17 {
                tx.execute_batch(include_str!("rank_apply.sql"))?;
            }
            if version < 18 {
                crate::catalog::migrate_opening_identities(&tx)?;
            }
            tx.execute_batch(include_str!("game_feedback.sql"))?;
            tx.pragma_update(None, "user_version", SCHEMA)?;
            tx.commit()?;
            return Ok(());
        }
        // Replace the referenced parent table without cascading child rows.
        // Check the complete replacement before committing the upgrade.
        db.pragma_update(None, "foreign_keys", false)?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch(include_str!("opening_order.sql"))?;
        if version < 1 {
            tx.execute_batch(include_str!("opening_policy.sql"))?;
        }
        if version < 2 {
            tx.execute_batch(include_str!("game_pipeline.sql"))?;
            crate::game_pipeline::recover_legacy(&tx)?;
        }
        if version < 3 {
            tx.execute_batch(include_str!("prepared_queue.sql"))?;
        }
        if version < 4 {
            tx.execute_batch(include_str!("job_turns.sql"))?;
        }
        if version < 5 {
            tx.execute_batch(include_str!("operation_bounds.sql"))?;
        }
        if version < 6 {
            tx.execute_batch(include_str!("job_admission.sql"))?;
        }
        tx.execute_batch(include_str!("coverage.sql"))?;
        tx.execute_batch(include_str!("coverage_order.sql"))?;
        tx.execute_batch(include_str!("gap_batches.sql"))?;
        crate::games::upgrade(&tx)?;
        tx.execute_batch(include_str!("detectors.sql"))?;
        tx.execute_batch(include_str!("lichess_packs.sql"))?;
        tx.execute_batch(include_str!("lichess_start_revision.sql"))?;
        tx.execute_batch(include_str!("card_availability.sql"))?;
        tx.execute_batch(include_str!("tactic_qualification.sql"))?;
        tx.execute_batch(include_str!("opening_replacement.sql"))?;
        upgrade_views(&tx)?;
        tx.execute_batch(include_str!("rank_apply.sql"))?;
        crate::catalog::migrate_opening_identities(&tx)?;
        tx.execute_batch(include_str!("game_feedback.sql"))?;
        let valid: bool = tx.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            valid,
            "foreign-key check failed after converting the archive"
        );
        tx.pragma_update(None, "user_version", SCHEMA)?;
        tx.commit()?;
        db.pragma_update(None, "foreign_keys", true)?;
        Ok(())
    }

    fn configure(db: &Connection) -> Result<()> {
        db.busy_timeout(Duration::from_secs(2))?;
        Self::configure_pragmas(db)
    }

    fn configure_pragmas(db: &Connection) -> Result<()> {
        db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF;")?;
        Ok(())
    }
}

pub fn dataset(db: &Connection) -> Result<String> {
    Ok(db.query_row("SELECT dataset FROM metadata", [], |r| r.get(0))?)
}

pub fn prior_review(db: &Connection, card: &str) -> Result<Option<crate::learning::PriorReview>> {
    use rusqlite::OptionalExtension;
    Ok(db
        .query_row(
            "SELECT stability,difficulty,last_at FROM learning WHERE card_id=?1",
            [card],
            |r| {
                Ok(crate::learning::PriorReview {
                    stability: r.get(0)?,
                    difficulty: r.get(1)?,
                    last_at: r.get(2)?,
                })
            },
        )
        .optional()?)
}

pub fn save_review(
    db: &Connection,
    card: &str,
    now: i64,
    schedule: crate::learning::ReviewSchedule,
) -> Result<()> {
    db.execute("INSERT INTO learning VALUES (?1,?2,?3,?4,?5)
        ON CONFLICT(card_id) DO UPDATE SET stability=excluded.stability,difficulty=excluded.difficulty,
        last_at=excluded.last_at,due_at=excluded.due_at",
        rusqlite::params![card,schedule.stability,schedule.difficulty,now,schedule.due_at])?;
    Ok(())
}

fn upgrade_views(db: &Connection) -> Result<()> {
    db.execute_batch(include_str!("capture_status.sql"))?;
    db.execute_batch(include_str!("coverage_views.sql"))?;
    crate::coverage::backfill_views(db)
}
