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
            return Self::open(path);
        }
        let db = Connection::open(path)?;
        Self::configure(&db)?;
        db.execute_batch("PRAGMA journal_mode=WAL; BEGIN IMMEDIATE;")?;
        db.execute_batch(include_str!("schema.sql"))?;
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
