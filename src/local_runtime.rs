//! File-backed composition for the local tactics page.
mod http;
mod import;

use std::{
    error::Error,
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use tempo::{AttemptIds, Clock, Components, Tempo};
use tempo_chess::{Catalog, LegalSequenceGrader, RepertoirePgnParser};
use tempo_learning::{FixedIntervalScheduler, StandardLearning};
use tempo_queue::StableQueue;
use tempo_store::SqliteStore;
use tempo_types::{AttemptId, DatasetId, StudyDay, StudyTime};

type LocalResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

pub fn init(path: &Path) -> LocalResult<()> {
    let components = components()?;
    SqliteStore::create(
        path,
        DatasetId(format!("practice-{}", random_id()?)),
        Catalog::default(),
        components.learning.settings(),
    )?;
    println!("Created a fresh V2 practice dataset at {}", path.display());
    Ok(())
}

pub fn import(path: &Path, puzzles: &Path) -> LocalResult<()> {
    let batch = import::Batch::open(puzzles)?;
    let mut app = open(path)?;
    import::run(&mut app, batch)
}

pub fn serve(database: &Path, ui: &Path, port: u16) -> LocalResult<()> {
    if fs::canonicalize(database)?.starts_with(fs::canonicalize(ui)?) {
        return Err("place the practice database outside the public UI build".into());
    }
    let mut app = open(database)?;
    http::serve(&mut app, ui, port)
}

fn open(path: &Path) -> LocalResult<Tempo> {
    let store = SqliteStore::open(path)?;
    Ok(Tempo::new(Box::new(store), components()?)?)
}

fn random_id() -> LocalResult<String> {
    let mut bytes = [0; 16];
    getrandom::getrandom(&mut bytes).map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(format!("{:032x}", u128::from_be_bytes(bytes)))
}

fn components() -> LocalResult<Components> {
    Ok(Components {
        queue: Box::new(StableQueue),
        learning: Box::new(StandardLearning::<FixedIntervalScheduler>::default()),
        grader: Box::new(LegalSequenceGrader),
        parser: Box::new(RepertoirePgnParser),
        comparator: Box::new(tempo_chess::demo_comparator()),
        clock: Box::new(UtcClock),
        ids: Box::new(LocalIds {
            run: random_id()?,
            sequence: 0,
        }),
    })
}

struct UtcClock;
impl Clock for UtcClock {
    fn now(&self) -> StudyTime {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        StudyTime {
            day: StudyDay(i32::try_from(elapsed.as_secs() / 86_400).unwrap_or(i32::MAX)),
            unix_ms: elapsed.as_millis() as u64,
        }
    }
}

struct LocalIds {
    run: String,
    sequence: u64,
}

impl AttemptIds for LocalIds {
    fn next_id(&mut self) -> AttemptId {
        self.sequence += 1;
        AttemptId(format!("attempt-{}-{}", self.run, self.sequence))
    }
}
