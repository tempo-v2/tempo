use anyhow::{Result, ensure};
use rusqlite::{Connection, OpenFlags};
use std::{path::Path, time::Duration};

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

    pub fn init(path: &Path) -> Result<Connection> {
        if path.exists() {
            let mut db = Self::open(path)?;
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute_batch(include_str!("opening_order.sql"))?;
            let version =
                tx.pragma_query_value(None, "user_version", |row| row.get::<_, i32>(0))?;
            ensure!(version <= 2, "this Tempo database requires a newer release");
            if version < 1 {
                tx.execute_batch(include_str!("opening_policy.sql"))?;
            }
            if version < 2 {
                tx.execute_batch(include_str!("game_pipeline.sql"))?;
                crate::game_pipeline::recover_legacy(&tx)?;
            }
            tx.commit()?;
            return Ok(db);
        }
        let db = Connection::open(path)?;
        Self::configure(&db)?;
        db.execute_batch("PRAGMA journal_mode=WAL; BEGIN IMMEDIATE;")?;
        db.execute_batch(include_str!("schema.sql"))?;
        db.execute_batch(include_str!("opening_policy.sql"))?;
        db.execute_batch(include_str!("opening_order.sql"))?;
        db.execute_batch(include_str!("game_pipeline.sql"))?;
        db.execute(
            "INSERT INTO metadata VALUES (?1)",
            [format!("{:032x}", rand::random::<u128>())],
        )?;
        db.execute_batch("PRAGMA application_id=1414352946; COMMIT;")?;
        Ok(db)
    }

    fn configure(db: &Connection) -> Result<()> {
        db.busy_timeout(Duration::from_secs(2))?;
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
