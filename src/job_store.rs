use crate::{
    catalog,
    db::Database,
    game_pipeline, games,
    jobs::{Job, JobView, StartJob, valid_user},
    providers::Provider,
    scanner,
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

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
        "SELECT id,provider,operation,status,added,existing,rejected,found,error FROM jobs
        WHERE id IN (SELECT id FROM jobs ORDER BY id DESC LIMIT 12)
           OR id IN (SELECT id FROM jobs WHERE status='failed' ORDER BY id DESC LIMIT 12)
        ORDER BY id DESC",
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

pub fn start(db: &mut Connection, input: StartJob, upload: Option<&str>) -> Result<i64> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let n: usize = tx.query_row(
        "SELECT count(*) FROM jobs WHERE status IN ('queued','running') AND parent_id IS NULL AND operation!='publish'",
        [],
        |r| r.get(0),
    )?;
    ensure!(n < 4, "the import queue is full");
    let username = match (input.provider.as_str(), input.operation.as_str()) {
        ("pgn", "upload") => {
            ensure!(upload.is_some(), "enter PGN up to 64 MiB");
            valid_user(input.username.as_deref().unwrap_or(""))?.to_owned()
        }
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
    let existing = if input.provider != "pgn" {
        tx.query_row("SELECT id FROM jobs WHERE provider=?1 AND operation=?2 AND username=?3 AND status IN ('queued','running') LIMIT 1",
            params![input.provider,input.operation,username],|r|r.get::<_,i64>(0)).optional()?
    } else {
        None
    };
    if let Some(job) = existing {
        return Ok(job);
    }
    tx.execute("INSERT INTO jobs(provider,username,operation,upload_file,status) VALUES (?1,?2,?3,?4,'queued')",
        params![input.provider,username,input.operation,upload])?;
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
        let n: usize = tx.query_row(
            "SELECT count(*) FROM jobs WHERE status IN ('queued','running') AND parent_id IS NULL AND operation!='publish'",
            [], |r| r.get(0),
        )?;
        ensure!(n < 4, "the import queue is full");
    }
    ensure!(
        tx.execute(
            "UPDATE jobs SET status='queued',error=NULL WHERE id=?1 AND status='failed'",
            [id]
        )? == 1,
        "the job is not waiting for retry"
    );
    tx.commit()?;
    Ok(())
}

pub fn next(db: &mut Connection) -> Result<Option<Job>> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let job = tx.query_row("SELECT id,provider,username,operation,cursor FROM jobs WHERE status='queued' ORDER BY id LIMIT 1",[],
        |r| Ok(Job { id:r.get(0)?,provider:r.get(1)?,username:r.get(2)?,operation:r.get(3)?,cursor:r.get(4)? })).optional()?;
    if let Some(job) = &job {
        tx.execute("UPDATE jobs SET status='running' WHERE id=?1", [job.id])?;
    }
    tx.commit()?;
    Ok(job)
}

pub fn recover_running(db: &mut Connection) -> Result<usize> {
    Ok(db.execute(
        "UPDATE jobs SET status='queued' WHERE id IN
        (SELECT id FROM jobs WHERE status='running' ORDER BY id LIMIT 128)",
        [],
    )?)
}

pub fn fail(db: &Connection, id: i64, error: &str) -> Result<()> {
    db.execute(
        "UPDATE jobs SET status='failed',error=?2 WHERE id=?1",
        params![id, error],
    )?;
    Ok(())
}

pub fn complete(
    db: &mut Connection,
    id: i64,
    provider: &str,
    username: &str,
    checkpoint: Option<i64>,
) -> Result<()> {
    let tx = db.transaction()?;
    if let Some(through) = checkpoint {
        tx.execute("INSERT INTO provider_checkpoint VALUES (?1,?2,?3)
            ON CONFLICT(provider,username) DO UPDATE SET through_at=max(through_at,excluded.through_at)",
            params![provider,username,through])?;
    }
    tx.execute(
        "UPDATE jobs SET status='done',payload=NULL,upload_file=NULL,error=NULL WHERE id=?1",
        [id],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn save_cursor(db: &Connection, id: i64, cursor: &str, rejected: usize) -> Result<()> {
    db.execute(
        "UPDATE jobs SET cursor=?2,rejected=rejected+?3 WHERE id=?1",
        params![id, cursor, rejected],
    )?;
    Ok(())
}

pub struct JobReads(Connection);
impl JobReads {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self(Database::read(path)?))
    }

    pub fn upload(&self, id: i64) -> Result<(Option<String>, Option<String>)> {
        Ok(self.0.query_row(
            "SELECT upload_file,payload FROM jobs WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
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

    pub fn archive(&self, cursor: &str) -> Result<Vec<games::Game>> {
        games::archive(&self.0, cursor, 25)
    }

    pub fn findings(&self, job: i64, game: &str) -> Result<Option<scanner::Findings>> {
        game_pipeline::scan_findings(&self.0, job, game)
    }

    pub fn pending_games(&self, job: i64) -> Result<Vec<String>> {
        game_pipeline::pending_games(&self.0, job)
    }

    pub fn publication_batch(&self, game: &str) -> Result<Vec<catalog::TacticPlan>> {
        game_pipeline::publication_batch(&self.0, game)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_job_remains_visible_alongside_prior_failures() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(include_str!("schema.sql"))?;
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
