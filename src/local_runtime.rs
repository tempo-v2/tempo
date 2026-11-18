//! File-backed composition for the matched local study release.
mod http;
mod import;
mod tactic_settings;

use chrono::{DateTime, NaiveDate};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use tempo::{AttemptIds, Clock, Components, IntroductionLimits, TacticThemes, Tempo};
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
    #[serde(default = "default_more_tactics")]
    more_tactics: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tactic_themes: Option<TacticThemes>,
}

fn default_more_tactics() -> usize {
    10
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            study_timezone: "America/New_York".into(),
            introductions: IntroductionLimits::default(),
            preview_key: [0; 32],
            more_tactics: default_more_tactics(),
            tactic_themes: None,
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

pub fn configure(
    path: &Path,
    repertoire: usize,
    tactics: usize,
    more: Option<usize>,
) -> LocalResult<()> {
    let store = SqliteStore::open(path)?;
    let previous = stored_profile(&store)?;
    let mut profile: Profile = serde_json::from_str(&previous)?;
    profile.introductions = IntroductionLimits {
        repertoire,
        tactics,
    };
    if let Some(more) = more {
        profile.more_tactics = more;
    }
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

pub fn backfill_personal_tactics(path: &Path, apply: bool) -> LocalResult<()> {
    let sources = SqliteStore::open(path)?.content_source_ids()?;
    let result = open(path)?.backfill_personal_tactics(&sources, apply)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

pub fn serve(database: &Path, ui: &Path, port: u16) -> LocalResult<()> {
    if fs::canonicalize(database)?.starts_with(fs::canonicalize(ui)?) {
        return Err("place the study database outside the public UI build".into());
    }
    let mut app = open(database)?;
    http::serve(&mut app, &SqliteStore::open(database)?, ui, port)
}

fn stored_profile(store: &SqliteStore) -> LocalResult<String> {
    store
        .local_profile()?
        .ok_or_else(|| "this database has no Tempo V2 study profile".into())
}

fn open(path: &Path) -> LocalResult<Tempo> {
    let store = SqliteStore::open(path)?;
    let profile: Profile = serde_json::from_str(&stored_profile(&store)?)?;
    let mut app = Tempo::daily(
        Box::new(store),
        components(profile.study_timezone.parse()?)?,
        profile.introductions,
        profile.preview_key,
        profile.more_tactics,
    )?;
    app.set_local_tactic_preferences(profile.introductions.tactics, profile.tactic_themes);
    Ok(app)
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
        study_time(elapsed.as_millis() as u64, self.0)
    }
}

fn study_time(unix_ms: u64, zone: Tz) -> StudyTime {
    let timestamp = DateTime::from_timestamp_millis(unix_ms as i64)
        .expect("system clock fits supported calendar");
    let local_date = timestamp.with_timezone(&zone).date_naive();
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid epoch");
    StudyTime {
        day: StudyDay(local_date.signed_duration_since(epoch).num_days() as i32),
        unix_ms,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn study_day_changes_at_new_york_midnight_across_dst() {
        let zone = chrono_tz::America::New_York;
        for (first, second, length) in [
            ("2024-03-10T05:00:00Z", "2024-03-11T04:00:00Z", 23),
            ("2024-11-03T04:00:00Z", "2024-11-04T05:00:00Z", 25),
        ] {
            let millis = |time| {
                DateTime::parse_from_rfc3339(time)
                    .unwrap()
                    .timestamp_millis() as u64
            };
            let first = millis(first);
            let second = millis(second);
            assert_eq!((second - first) / 3_600_000, length);
            assert_eq!(
                study_time(first - 1, zone).day.0 + 1,
                study_time(first, zone).day.0
            );
            assert_eq!(
                study_time(first, zone).day,
                study_time(second - 1, zone).day
            );
            assert_eq!(
                study_time(second - 1, zone).day.0 + 1,
                study_time(second, zone).day.0
            );
        }
    }
}
