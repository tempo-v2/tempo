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
    generation: u64,
    reconciled: Option<u64>,
    reservations: HashMap<String, (usize, Weak<()>)>,
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
                loop {
                    if let Err(error) = cleaner.reconcile() {
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
            ensure!(
                inventory.reconciled == Some(inventory.generation)
                    && inventory.reservations.len() < FILES
                    && inventory
                        .reservations
                        .values()
                        .map(|(size, _)| size)
                        .sum::<usize>()
                        + bytes.len()
                        <= BYTES
                    && !inventory.reservations.contains_key(&id),
                Unavailable
            );
            inventory
                .reservations
                .insert(id.clone(), (bytes.len(), Arc::downgrade(&lease)));
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

    /// A checkpoint released a durable reference. Reconcile it off the worker.
    pub fn released(&self) {
        self.inventory.lock().unwrap().generation += 1;
    }

    fn reconcile(&self) -> Result<()> {
        let (scan, released) = {
            let inventory = self.inventory.lock().unwrap();
            let scan = (inventory.reconciled != Some(inventory.generation))
                .then_some(inventory.generation);
            let released: Vec<_> = inventory
                .reservations
                .iter()
                .filter(|(_, (_, lease))| lease.strong_count() == 0)
                .map(|(id, _)| id.clone())
                .collect();
            (scan, released)
        };
        if scan.is_none() && released.is_empty() {
            return Ok(());
        }
        let read = Database::read_with_busy_timeout(&self.database, Duration::ZERO)?;
        let mut used =
            read.prepare_cached("SELECT EXISTS(SELECT 1 FROM jobs WHERE upload_file=?1)")?;
        let mut reconcile_file = |id: &str| -> Result<()> {
            if self
                .inventory
                .lock()
                .unwrap()
                .reservations
                .get(id)
                .is_some_and(|(_, lease)| lease.strong_count() != 0)
            {
                return Ok(());
            }
            if !used.query_row([id], |row| row.get::<_, bool>(0))? {
                match fs::remove_file(path(&self.database, id)?) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            self.inventory.lock().unwrap().reservations.remove(id);
            Ok(())
        };
        // Try independent released leases even if one file cannot be removed.
        let mut error = None;
        for id in released {
            if let Err(failure) = reconcile_file(&id) {
                error = Some(failure);
            }
        }
        if let Some(generation) = scan {
            match fs::read_dir(directory(&self.database)) {
                Ok(entries) => {
                    for (index, entry) in entries.enumerate() {
                        let entry = entry?;
                        if let Some(id) = entry.file_name().to_str()
                            && path(&self.database, id).is_ok()
                            && entry.file_type()?.is_file()
                            && let Err(failure) = reconcile_file(id)
                        {
                            error = Some(failure);
                        }
                        if index % 32 == 31 {
                            std::thread::yield_now();
                        }
                    }
                }
                Err(failure) if failure.kind() == io::ErrorKind::NotFound => {}
                Err(failure) => return Err(failure.into()),
            }
            if error.is_none() {
                self.inventory.lock().unwrap().reconciled = Some(generation);
            }
        }
        error.map_or(Ok(()), Err)
    }
}

fn path(db: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid upload identity"
    );
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

    #[test]
    fn failed_reclamation_neither_owns_writer_nor_exceeds_staging_quota() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("tempo-stage-{:032x}", rand::random::<u128>()));
        fs::create_dir(&dir)?;
        let database = dir.join("fixture.sqlite3");
        let result = (|| -> Result<()> {
            let mut db = Database::init(&database)?;
            let staging = Staging {
                database: Arc::new(database.clone()),
                inventory: Arc::new(Mutex::new(Inventory::default())),
            };
            staging.reconcile()?;
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
            }
            assert!(staging.reconcile().is_err());
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
            staging.reconcile()?;
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
            staging.reconcile()?;
            assert!(path(&database, &durable)?.is_file());
            Ok(())
        })();
        fs::remove_dir_all(dir)?;
        result
    }
}
