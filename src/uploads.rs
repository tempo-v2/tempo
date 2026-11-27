use crate::db::Database;
use anyhow::{Result, ensure};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

fn directory(db: &Path) -> PathBuf {
    db.with_extension("uploads")
}

const FILES: usize = 4;
const BYTES: usize = 256 * 1024 * 1024;
const WORKER_BYTES: usize = 32 * 1024 * 1024;
const STARTUP_BATCH: usize = 32;

#[derive(Debug)]
pub struct Unavailable;

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Games file staging is busy. Retry.")
    }
}

impl std::error::Error for Unavailable {}

/// A writer proposal owns only this ID and its memory-only lease.
pub struct Staged {
    id: String,
    _lease: Arc<()>,
}

impl Staged {
    pub fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Default)]
struct Inventory {
    initialized: bool,
    reservations: HashMap<String, (usize, Weak<()>)>,
}

impl Inventory {
    fn insert(&mut self, id: &str, bytes: usize, lease: Weak<()>) -> Result<()> {
        let reserved: usize = self.reservations.values().map(|(size, _)| size).sum();
        ensure!(
            self.reservations.len() < FILES
                && bytes <= BYTES - reserved
                && !self.reservations.contains_key(id),
            Unavailable
        );
        self.reservations.insert(id.to_owned(), (bytes, lease));
        Ok(())
    }
}

#[derive(Default)]
struct Startup {
    entries: Option<fs::ReadDir>,
    pending: Option<PathBuf>,
}

#[derive(Clone)]
pub struct Staging {
    database: Arc<PathBuf>,
    inventory: Arc<Mutex<Inventory>>,
}

impl Staging {
    pub fn start(database: &Path) -> Result<Self> {
        let staging = Self {
            database: Arc::new(database.to_owned()),
            inventory: Arc::new(Mutex::new(Inventory::default())),
        };
        let cleaner = staging.clone();
        std::thread::Builder::new()
            .name("tempo-staged-file-cleanup".into())
            .spawn(move || {
                let mut startup = Startup::default();
                loop {
                    if let Err(error) = cleaner.reconcile(&mut startup) {
                        eprintln!("Games file cleanup will retry: {error:#}");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            })?;
        Ok(staging)
    }

    pub fn manual(&self, pgn: &str) -> Result<Staged> {
        ensure!(
            !pgn.trim().is_empty() && pgn.len() <= crate::jobs::MAX_PGN,
            "enter PGN up to 64 MiB"
        );
        self.stage(pgn.as_bytes())
    }

    pub fn worker(&self, bytes: &[u8]) -> Result<Staged> {
        ensure!(bytes.len() <= WORKER_BYTES, "Games staging exceeds 32 MiB");
        self.stage(bytes)
    }

    fn stage(&self, bytes: &[u8]) -> Result<Staged> {
        let id = format!("{:032x}", rand::random::<u128>());
        let lease = Arc::new(());
        {
            let mut inventory = self.inventory.lock().unwrap();
            ensure!(inventory.initialized, Unavailable);
            inventory.insert(&id, bytes.len(), Arc::downgrade(&lease))?;
        }
        let dir = directory(&self.database);
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&dir)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(&id))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        File::open(dir)?.sync_all()?;
        Ok(Staged { id, _lease: lease })
    }

    pub fn open(&self, id: &str, deadline: Instant) -> Result<StagedInput> {
        ensure!(Instant::now() < deadline, "Games turn timed out.");
        Ok(StagedInput(
            File::open(path(&self.database, id)?)?,
            deadline,
        ))
    }

    /// Hold this file in quota through the transaction that clears its job reference.
    pub fn reserve_release(&self, id: &str) -> Result<Staged> {
        ensure!(valid_id(id), "invalid upload identity");
        let mut inventory = self.inventory.lock().unwrap();
        ensure!(inventory.initialized, Unavailable);
        let lease = match inventory.reservations.get(id) {
            // An expired lease belongs to the cleaner until its check finishes.
            Some((_, lease)) => lease.upgrade().ok_or(Unavailable)?,
            None => {
                let lease = Arc::new(());
                inventory.insert(id, crate::jobs::MAX_PGN, Arc::downgrade(&lease))?;
                lease
            }
        };
        Ok(Staged {
            id: id.to_owned(),
            _lease: lease,
        })
    }

    fn reconcile(&self, startup: &mut Startup) -> Result<()> {
        let (initializing, released) = {
            let inventory = self.inventory.lock().unwrap();
            let released: Vec<_> = inventory
                .reservations
                .iter()
                .filter(|(_, (_, lease))| lease.strong_count() == 0)
                .map(|(id, _)| id.clone())
                .collect();
            (!inventory.initialized, released)
        };
        if !initializing && released.is_empty() {
            return Ok(());
        }
        let read = Database::read_with_busy_timeout(&self.database, Duration::ZERO)?;
        let mut used =
            read.prepare_cached("SELECT EXISTS(SELECT 1 FROM jobs WHERE upload_file=?1)")?;
        // Try independent released leases even if one file cannot be removed.
        let mut error = None;
        for id in released {
            match referenced(&mut used, &id).and_then(|referenced| {
                if !referenced {
                    unlink(&directory(&self.database).join(&id))?;
                }
                Ok(())
            }) {
                Ok(()) => {
                    self.inventory.lock().unwrap().reservations.remove(&id);
                }
                Err(failure) => error = Some(failure),
            }
        }
        if initializing {
            self.reconcile_startup(startup, &mut used)?;
        }
        error.map_or(Ok(()), Err)
    }

    fn reconcile_startup(
        &self,
        startup: &mut Startup,
        used: &mut rusqlite::Statement<'_>,
    ) -> Result<()> {
        if startup.entries.is_none() {
            match fs::read_dir(directory(&self.database)) {
                Ok(entries) => startup.entries = Some(entries),
                Err(failure) if failure.kind() == io::ErrorKind::NotFound => {
                    self.inventory.lock().unwrap().initialized = true;
                    return Ok(());
                }
                Err(failure) => return Err(failure.into()),
            }
        }
        for _ in 0..STARTUP_BATCH {
            if startup.pending.is_none() {
                match startup.entries.as_mut().unwrap().next() {
                    Some(Ok(entry)) => startup.pending = Some(entry.path()),
                    Some(Err(failure)) => {
                        startup.entries = None;
                        return Err(failure.into());
                    }
                    None => {
                        startup.entries = None;
                        self.inventory.lock().unwrap().initialized = true;
                        return Ok(());
                    }
                }
            }
            self.reconcile_startup_file(startup.pending.as_ref().unwrap(), used)?;
            startup.pending = None;
        }
        Ok(())
    }

    fn reconcile_startup_file(
        &self,
        file: &Path,
        used: &mut rusqlite::Statement<'_>,
    ) -> Result<()> {
        let Some(id) = file
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|id| valid_id(id))
        else {
            return Ok(());
        };
        let metadata = match file.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(failure) if failure.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(failure) => return Err(failure.into()),
        };
        let tracked = self.inventory.lock().unwrap().reservations.contains_key(id);
        if metadata.is_file()
            && !tracked
            && !referenced(used, id)?
            && let Err(failure) = unlink(file)
        {
            // A known orphan can remain in service if it fits the existing quota.
            self.inventory.lock().unwrap().insert(
                id,
                usize::try_from(metadata.len())?,
                Weak::new(),
            )?;
            eprintln!("Games file cleanup will retry: {failure:#}");
        }
        Ok(())
    }
}

fn referenced(used: &mut rusqlite::Statement<'_>, id: &str) -> Result<bool> {
    Ok(used.query_row([id], |row| row.get(0))?)
}

fn unlink(file: &Path) -> io::Result<()> {
    match fs::remove_file(file) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn path(db: &Path, id: &str) -> Result<PathBuf> {
    ensure!(valid_id(id), "invalid upload identity");
    Ok(directory(db).join(id))
}

/// File reads during parsing observe the original monotonic turn deadline.
pub struct StagedInput(File, Instant);

impl Read for StagedInput {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        let n = self.0.read(bytes)?;
        self.check()?;
        Ok(n)
    }
}

impl Seek for StagedInput {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.check()?;
        let offset = self.0.seek(position)?;
        self.check()?;
        Ok(offset)
    }
}

impl StagedInput {
    fn check(&self) -> io::Result<()> {
        if Instant::now() < self.1 {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Games turn timed out.",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::Writer;

    fn fixture(database: &Path) -> Staging {
        Staging {
            database: Arc::new(database.to_owned()),
            inventory: Arc::new(Mutex::new(Inventory::default())),
        }
    }

    #[test]
    fn startup_checks_a_bounded_page_and_keeps_referenced_files() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("db");
        let db = Database::init(&database)?;
        let staging = fixture(&database);
        let mut startup = Startup::default();
        let files = super::directory(&database);
        fs::create_dir(&files)?;
        let retained = format!("{:032x}", STARTUP_BATCH);
        for index in 0..=STARTUP_BATCH {
            fs::write(files.join(format!("{index:032x}")), b"fixture")?;
        }
        db.execute(
            "INSERT INTO jobs(provider,username,operation,status,upload_file)
             VALUES ('pgn','Learner','upload','failed',?1)",
            [&retained],
        )?;
        staging.reconcile(&mut startup)?;
        assert!(
            staging
                .worker(b"fixture")
                .err()
                .unwrap()
                .is::<Unavailable>()
        );
        assert!(files.join(&retained).is_file());
        staging.reconcile(&mut startup)?;
        assert_eq!(fs::read_dir(&files)?.count(), 1);
        assert!(files.join(&retained).is_file());
        assert!(staging.worker(b"fixture").is_ok());
        Ok(())
    }

    #[test]
    fn job_release_holds_quota_until_commit_and_checks_only_its_file() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("db");
        let mut db = Database::init(&database)?;
        let staging = fixture(&database);
        let mut startup = Startup::default();
        staging.reconcile(&mut startup)?;

        let staged = staging.worker(b"fixture")?;
        let id = staged.id().to_owned();
        let file = path(&database, &id)?;
        db.execute(
            "INSERT INTO jobs(provider,username,operation,status,upload_file)
             VALUES ('pgn','Learner','upload','running',?1)",
            [&id],
        )?;
        let job = db.last_insert_rowid();
        drop(staged);
        staging.reconcile(&mut startup)?;

        {
            let _release = staging.reserve_release(&id)?;
            let tentative = db.transaction()?;
            tentative.execute("UPDATE jobs SET upload_file=NULL WHERE id=?1", [job])?;
            tentative.rollback()?;
        }
        staging.reconcile(&mut startup)?;
        assert!(file.is_file());

        let unrelated = path(&database, "00000000000000000000000000000000")?;
        fs::write(
            &unrelated,
            b"a post-startup directory sweep would read this file",
        )?;
        let release = staging.reserve_release(&id)?;
        crate::job_store::complete(&mut db, job, "pgn", "learner", None)?;
        staging.reconcile(&mut startup)?;
        assert!(file.is_file());
        drop(release);
        staging.reconcile(&mut startup)?;
        assert!(!file.exists());
        assert!(unrelated.is_file());
        Ok(())
    }

    #[test]
    fn failed_reclamation_neither_owns_writer_nor_exceeds_staging_quota() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("tempo-stage-{:032x}", rand::random::<u128>()));
        fs::create_dir(&dir)?;
        let database = dir.join("fixture.sqlite3");
        let result = (|| -> Result<()> {
            let mut db = Database::init(&database)?;
            let staging = fixture(&database);
            let mut startup = Startup::default();
            staging.reconcile(&mut startup)?;
            let mut obstacles = Vec::new();
            for _ in 0..FILES {
                let file = staging.worker(b"fixture")?;
                let name = path(&database, file.id())?;
                fs::remove_file(&name)?;
                fs::create_dir(&name)?;
                // Dropping this lease must not attempt to unlink the file.
                let tx = db.transaction()?;
                drop(file);
                tx.commit()?;
                obstacles.push(name);
                assert!(staging.reconcile(&mut startup).is_err());
            }
            assert!(
                staging
                    .worker(b"one too many")
                    .err()
                    .unwrap()
                    .is::<Unavailable>()
            );
            let writer = Writer::start(db, None)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let accepted = runtime.block_on(async {
                writer
                    .submit(false, |connection| {
                        let route = crate::chess::Route {
                            fen: crate::chess::fen(&crate::chess::position("")?),
                            moves: vec!["e2e4".into()],
                        };
                        let plan = crate::catalog::prepare_tactic(crate::catalog::TacticInput {
                            name: "Fixture",
                            route: &route,
                            answers: &route.moves,
                            source: None,
                        })?;
                        crate::catalog::apply_tactic(connection, &plan)?;
                        crate::queue_store::seed_batch(connection)?;
                        crate::daily_store::prepare_test_day(
                            connection,
                            chrono::Utc::now().timestamp(),
                        )?;
                        let attempt = crate::review::next(
                            connection,
                            crate::study::Next {
                                after: None,
                                dataset: None,
                            },
                        )?
                        .unwrap();
                        crate::review::answer(
                            connection,
                            crate::study::AnswerIntentV1 {
                                attempt: attempt.attempt,
                                dataset: attempt.dataset,
                                action: crate::study::AnswerAction::Play("e2e4".into()),
                            },
                        )
                    })?
                    .wait()
                    .await
            })?;
            assert_eq!(serde_json::to_value(accepted)?["rating"], "Good");
            for obstacle in obstacles {
                fs::remove_dir(obstacle)?;
            }
            staging.reconcile(&mut startup)?;
            let live = staging.worker(b"retained")?;
            let durable = live.id().to_owned();
            runtime.block_on(async {
                let referenced = durable.clone();
                writer
                    .submit(false, move |connection| {
                        connection.execute(
                            "INSERT INTO jobs(provider,username,operation,status,upload_file)
                        VALUES ('pgn','Learner','upload','queued',?1)",
                            [referenced],
                        )?;
                        Ok(())
                    })?
                    .wait()
                    .await
            })?;
            drop(live);
            staging.reconcile(&mut startup)?;
            assert!(path(&database, &durable)?.is_file());
            Ok(())
        })();
        fs::remove_dir_all(dir)?;
        result
    }
}
