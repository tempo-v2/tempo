//! File-backed composition for the matched local study release.
mod http;
mod import;

use chrono::{DateTime, NaiveDate};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use tempo::{AttemptIds, Clock, Components, IntroductionLimits, Tempo};
use tempo_chess::{
    Catalog, ChessError, Deviation, GameComparator, LegalSequenceGrader, RepertoirePgnParser,
};
use tempo_learning::DailyLearning;
use tempo_queue::StableQueue;
use tempo_store::SqliteStore;
use tempo_types::{AttemptId, DatasetId, StudyDay, StudyTime};

type LocalResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    study_timezone: String,
    introductions: IntroductionLimits,
    preview_key: [u8; 32],
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            study_timezone: "America/New_York".into(),
            introductions: IntroductionLimits::default(),
            preview_key: [0; 32],
        }
    }
}

pub fn init(path: &Path) -> LocalResult<()> {
    let mut profile = Profile::default();
    getrandom::getrandom(&mut profile.preview_key)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let components = components(profile.study_timezone.parse()?)?;
    let dataset = DatasetId(format!("study-{}", random_id()?));
    SqliteStore::create_with_profile(
        path,
        dataset.clone(),
        Catalog::default(),
        components.learning.settings(),
        Some(&serde_json::to_string(&profile)?),
    )?;
    println!("Created dataset {} at {}", dataset.0, path.display());
    Ok(())
}

pub fn configure(path: &Path, repertoire: usize, tactics: usize) -> LocalResult<()> {
    let store = SqliteStore::open(path)?;
    let previous = stored_profile(&store)?;
    let mut profile: Profile = serde_json::from_str(&previous)?;
    profile.introductions = IntroductionLimits {
        repertoire,
        tactics,
    };
    store.update_local_profile(&previous, &serde_json::to_string(&profile)?)?;
    println!(
        "Saved daily introduction limits of {repertoire} opening positions and {tactics} tactics. Restart Tempo to apply them."
    );
    Ok(())
}

pub fn import(path: &Path, puzzles: &Path) -> LocalResult<()> {
    let batch = import::Batch::open(puzzles)?;
    let mut app = open(path)?;
    import::run(&mut app, batch)
}

pub fn serve(database: &Path, ui: &Path, port: u16) -> LocalResult<()> {
    if fs::canonicalize(database)?.starts_with(fs::canonicalize(ui)?) {
        return Err("place the study database outside the public UI build".into());
    }
    let mut app = open(database)?;
    http::serve(&mut app, ui, port)
}

fn stored_profile(store: &SqliteStore) -> LocalResult<String> {
    store
        .local_profile()?
        .ok_or_else(|| "this database has no Tempo V2 study profile".into())
}

fn open(path: &Path) -> LocalResult<Tempo> {
    let store = SqliteStore::open(path)?;
    let profile: Profile = serde_json::from_str(&stored_profile(&store)?)?;
    Ok(Tempo::daily(
        Box::new(store),
        components(profile.study_timezone.parse()?)?,
        profile.introductions,
        profile.preview_key,
    )?)
}

fn random_id() -> LocalResult<String> {
    let mut bytes = [0; 16];
    getrandom::getrandom(&mut bytes).map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(format!("{:032x}", u128::from_be_bytes(bytes)))
}

fn components(timezone: Tz) -> LocalResult<Components> {
    Ok(Components {
        queue: Box::new(StableQueue),
        learning: Box::new(DailyLearning::default()),
        grader: Box::new(LegalSequenceGrader),
        parser: Box::new(RepertoirePgnParser),
        comparator: Box::new(NoGameComparison),
        clock: Box::new(LocalClock(timezone)),
        ids: Box::new(LocalIds {
            run: random_id()?,
            sequence: 0,
        }),
    })
}

struct NoGameComparison;

impl GameComparator for NoGameComparison {
    fn compare(&self, _: &str, _: &Catalog) -> Result<Vec<Deviation>, ChessError> {
        Err(ChessError::InvalidContent(
            "Game comparison is unavailable in this release.".into(),
        ))
    }
}

struct LocalClock(Tz);

impl Clock for LocalClock {
    fn now(&self) -> StudyTime {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let timestamp = DateTime::from_timestamp(elapsed.as_secs() as i64, elapsed.subsec_nanos())
            .expect("system clock fits supported calendar");
        let local_date = timestamp.with_timezone(&self.0).date_naive();
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid epoch");
        StudyTime {
            day: StudyDay(local_date.signed_duration_since(epoch).num_days() as i32),
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
