//! On-demand opening coverage, independent of Study reach projections.
use crate::{
    AppError, Tempo,
    app_error::domain,
    explorer::{ExplorerClient, Ratings},
    game_evidence::check_id,
    game_model::model_input_stamp,
    maia::{Distribution, MODEL_KEY, Maia, checked_distribution},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tempo_chess::{
    AuthoredRepertoire, LearnerColor, LearnerResponse, PositionKey, legal_successors,
};
use tempo_store::{CoverageLineSetting, CoverageSummary, SqliteStore, Store};
use tempo_types::DatasetId;

const EXPLORER_LIMIT: usize = 128;
const POSITION_LIMIT: usize = 2_000;
const RUN_LIMIT: Duration = Duration::from_secs(300);
const EXPLORER_TTL: Duration = Duration::from_secs(3600);
const EPSILON: f64 = 1e-9;
type Counts = (u64, BTreeMap<String, u64>);
type GapMass = (PositionKey, String, f64);
type ExplorerCache = Mutex<BTreeMap<PositionKey, (Instant, Counts)>>;

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartCoverage {
    request_id: String,
    prior_report_id: Option<String>,
    dataset_id: DatasetId,
    repertoire_id: String,
    through_move: u8,
    target_percent: u8,
    #[serde(default)]
    starting_line: CoverageLineSetting,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadCoverage {
    dataset_id: DatasetId,
    repertoire_id: String,
}

struct Run {
    request: StartCoverage,
    status: &'static str,
    positions: usize,
    error: Option<AppError>,
}

pub(crate) struct CoverageScan {
    database: PathBuf,
    explorer: Arc<ExplorerClient>,
    cache: Arc<ExplorerCache>,
    run: Arc<Mutex<Option<Run>>>,
}

impl CoverageScan {
    pub(crate) fn new(database: &Path, explorer: Arc<ExplorerClient>) -> Self {
        Self {
            database: database.to_owned(),
            explorer,
            cache: Default::default(),
            run: Default::default(),
        }
    }

    pub(crate) fn read(&self, app: &Tempo, request: ReadCoverage) -> Result<Value, AppError> {
        if app.game_dataset()? != request.dataset_id {
            return Err(stale_dataset());
        }
        let run = self.run.lock().expect("coverage scan");
        let starting_line = app.saved_coverage_line(&request.dataset_id, &request.repertoire_id)?;
        let saved = app.coverage_summary(&request.dataset_id, &request.repertoire_id)?;
        let active = run.as_ref().filter(|run| {
            run.request.dataset_id == request.dataset_id
                && run.request.repertoire_id == request.repertoire_id
        });
        let busy = run.as_ref().is_some_and(|run| run.status == "running");
        Ok(json!({
            "dataset_id": request.dataset_id, "repertoire_id": request.repertoire_id,
            "starting_line": starting_line,
            "status": active.map_or(if saved.is_some() { "complete" } else { "idle" }, |run| run.status),
            "busy": busy,
            "request_id": active.map(|run| &run.request.request_id),
            "positions": active.map(|run| run.positions),
            "error": active.and_then(|run| run.error.as_ref()).map(|error| &error.message),
            "report": saved.as_ref().map(|saved| &saved.summary),
            "stale": saved.is_some_and(|saved| saved.stale),
        }))
    }

    pub(crate) fn start(&self, app: &Tempo, request: StartCoverage) -> Result<Value, AppError> {
        check_id(&request.request_id)?;
        if app.game_dataset()? != request.dataset_id {
            return Err(stale_dataset());
        }
        if !(1..=20).contains(&request.through_move) || !(1..=100).contains(&request.target_percent)
        {
            return Err(AppError::new(
                "invalid_scan",
                "Choose 1–20 moves and a target of 1–100%.",
            ));
        }
        let address = ReadCoverage {
            dataset_id: request.dataset_id.clone(),
            repertoire_id: request.repertoire_id.clone(),
        };
        {
            let mut active = self.run.lock().expect("coverage scan");
            if let Some(prior) = active.as_ref() {
                if prior.request.request_id == request.request_id {
                    if prior.request != request {
                        return Err(AppError::new(
                            "invalid_scan",
                            "This scan ID was used for different inputs.",
                        ));
                    }
                    drop(active);
                    return self.read(app, address);
                }
                if prior.status == "running" {
                    return Err(AppError::new(
                        "coverage_busy",
                        "A repertoire scan is already running.",
                    ));
                }
            }
            let saved = app.coverage_summary(&request.dataset_id, &request.repertoire_id)?;
            if let Some(saved) = saved.as_ref().map(|saved| &saved.summary)
                && saved.request_id == request.request_id
            {
                if saved.target_percent != request.target_percent
                    || saved.through_move != request.through_move
                    || saved.starting_line != request.starting_line
                {
                    return Err(AppError::new(
                        "invalid_scan",
                        "This scan ID was used for different inputs.",
                    ));
                }
                drop(active);
                return self.read(app, address);
            }
            if saved.as_ref().map(|saved| &saved.summary.request_id)
                != request.prior_report_id.as_ref()
            {
                return Err(AppError::new(
                    "stale_coverage",
                    "The latest scan changed. Reload its status before starting again.",
                ));
            }
            let current = app.saved_coverage_line(&request.dataset_id, &request.repertoire_id)?;
            if current != request.starting_line {
                return Err(AppError::new(
                    "stale_coverage",
                    "The starting line changed. Reload the scan.",
                ));
            }
            *active = Some(Run {
                request: request.clone(),
                status: "running",
                positions: 0,
                error: None,
            });
        }
        let database = self.database.clone();
        let explorer = Arc::clone(&self.explorer);
        let cache = Arc::clone(&self.cache);
        let run = Arc::clone(&self.run);
        let context = crate::diagnostics::capture_context();
        if let Err(error) = std::thread::Builder::new()
            .name("tempo-repertoire-coverage".into())
            .spawn(move || {
                crate::diagnostics::with_context(context, || {
                    let result = scan(&database, &request, &explorer, &cache, |positions| {
                        run.lock()
                            .expect("coverage scan")
                            .as_mut()
                            .unwrap()
                            .positions = positions;
                    });
                    let mut state = run.lock().expect("coverage scan");
                    let finished = state.as_mut().unwrap();
                    finished.status = if result.is_ok() { "complete" } else { "failed" };
                    finished.error = result.err();
                });
            })
        {
            *self.run.lock().expect("coverage scan") = None;
            return Err(domain("coverage_unavailable", error));
        }
        self.read(app, address)
    }
}

fn stale_dataset() -> AppError {
    AppError::new("stale_dataset", "The active dataset changed.")
}

fn scan(
    database: &Path,
    request: &StartCoverage,
    explorer: &ExplorerClient,
    cache: &ExplorerCache,
    progress: impl Fn(usize),
) -> Result<(), AppError> {
    let _trace = crate::diagnostics::Span::new("coverage.scan");
    let started = Instant::now();
    let mut store = SqliteStore::open(database)?;
    let book = store
        .coverage_book(&request.dataset_id, &request.repertoire_id)?
        .ok_or_else(|| {
            AppError::new(
                "coverage_unavailable",
                "Choose an active repertoire with a verified PGN.",
            )
        })?;
    let authored = AuthoredRepertoire::parse(&book.pgn)
        .map_err(|error| domain("invalid_repertoire", error))?;
    let color = book
        .targets
        .values()
        .next()
        .and_then(|(_, item)| item.learner_color().ok())
        .ok_or_else(|| {
            AppError::new(
                "coverage_unavailable",
                "This repertoire has no saved responses.",
            )
        })?;
    if model_input_stamp(&authored, color, book.targets).key != book.input_key {
        return Err(AppError::new(
            "stale_repertoire",
            "The repertoire changed before the scan started.",
        ));
    }
    let mut sources = Sources::new(started + RUN_LIMIT, &store, explorer, cache, database);
    let estimate = estimate(
        &authored,
        color,
        &request.starting_line.moves,
        request.through_move,
        |position, moves| {
            let result = sources.distribution(position, moves);
            progress(sources.positions);
            result
        },
    )?;
    if estimate.unresolved >= 1.0 - EPSILON {
        return Err(AppError::new(
            "coverage_unavailable",
            "Explorer and Maia supplied no usable move probabilities for this scan.",
        ));
    }
    let summary = CoverageSummary {
        starting_san: authored
            .coverage_start(&request.starting_line.moves, color)
            .map_err(|error| domain("invalid_scan", error))?
            .san,
        starting_line: request.starting_line.clone(),
        request_id: request.request_id.clone(),
        through_move: request.through_move,
        target_percent: request.target_percent,
        covered: estimate.covered.clamp(0.0, 1.0),
        unresolved: estimate
            .unresolved
            .clamp(0.0, 1.0 - estimate.covered.clamp(0.0, 1.0)),
        positions: sources.positions,
        explorer_positions: sources.explorer_positions,
        maia_positions: sources.maia_positions,
        scanned_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    };
    drop(sources);
    let gaps = estimate.suggestions(request.target_percent);
    store.publish_coverage(
        &request.dataset_id,
        &request.repertoire_id,
        &book.input_key,
        request.prior_report_id.as_deref(),
        &summary,
        &gaps,
    )?;
    Ok(())
}

struct Sources<'a> {
    deadline: Instant,
    store: &'a SqliteStore,
    explorer: &'a ExplorerClient,
    cache: &'a ExplorerCache,
    directory: PathBuf,
    engine: Option<Maia>,
    engine_failed: bool,
    explorer_blocked: bool,
    lookups: usize,
    explorer_positions: usize,
    maia_positions: usize,
    positions: usize,
}

impl<'a> Sources<'a> {
    fn new(
        deadline: Instant,
        store: &'a SqliteStore,
        explorer: &'a ExplorerClient,
        cache: &'a ExplorerCache,
        database: &Path,
    ) -> Self {
        Self {
            deadline,
            store,
            explorer,
            cache,
            directory: database.with_file_name("maia"),
            engine: None,
            engine_failed: false,
            explorer_blocked: false,
            lookups: 0,
            explorer_positions: 0,
            maia_positions: 0,
            positions: 0,
        }
    }

    fn distribution(&mut self, position: &PositionKey, legal: &[String]) -> Distribution {
        if self.positions >= POSITION_LIMIT || Instant::now() >= self.deadline {
            return Distribution::new();
        }
        self.positions += 1;
        let counts = self.explorer_counts(position, legal);
        let maia = self.maia(position);
        if Instant::now() >= self.deadline {
            return Distribution::new();
        }
        self.explorer_positions +=
            usize::from(counts.as_ref().is_some_and(|(total, _)| *total > 0));
        self.maia_positions += usize::from(maia.is_some());
        blend(counts, maia)
    }

    fn explorer_counts(&mut self, position: &PositionKey, legal: &[String]) -> Option<Counts> {
        let cached = self
            .cache
            .lock()
            .expect("coverage Explorer cache")
            .get(position)
            .filter(|(when, _)| when.elapsed() < EXPLORER_TTL)
            .map(|(_, counts)| counts.clone());
        if cached.is_some() || self.explorer_blocked || self.lookups >= EXPLORER_LIMIT {
            return cached;
        }
        self.lookups += 1;
        let fen = format!("{} 0 1", position.0);
        let Ok(stats) = self.explorer.position_until(
            &fen,
            legal,
            Some(Ratings::Around1500),
            Some(self.deadline),
        ) else {
            self.explorer_blocked = true;
            return None;
        };
        let counts = stats.move_counts();
        let mut cache = self.cache.lock().expect("coverage Explorer cache");
        if cache.len() >= 512
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (when, _))| *when)
                .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(position.clone(), (Instant::now(), counts.clone()));
        Some(counts)
    }

    fn maia(&mut self, position: &PositionKey) -> Option<Distribution> {
        if let Ok(Some(moves)) = self.store.game_move_distribution(MODEL_KEY, &position.0)
            && let Ok(moves) = checked_distribution(position, moves)
        {
            return Some(moves);
        }
        if self.engine_failed || Instant::now() >= self.deadline {
            return None;
        }
        if self.engine.is_none() {
            self.engine = Maia::start_until(&self.directory, self.deadline).ok();
        }
        let result = self
            .engine
            .as_mut()
            .and_then(|engine| engine.infer_until(position, self.deadline).ok());
        if result.is_none() {
            self.engine_failed = true;
            self.engine = None;
        }
        result
    }
}

fn blend(explorer: Option<Counts>, maia: Option<Distribution>) -> Distribution {
    const PRIOR: f64 = 100.0;
    // The adapter tolerates small rounding errors in otherwise valid maps.
    let maia = maia.map(|mut moves| {
        let total: f64 = moves.values().sum();
        for probability in moves.values_mut() {
            *probability /= total;
        }
        moves
    });
    let Some((total, counts)) = explorer.filter(|(total, _)| *total > 0) else {
        return maia.unwrap_or_default();
    };
    let Some(maia) = maia else {
        return counts
            .into_iter()
            .map(|(played, count)| (played, count as f64 / total as f64))
            .collect();
    };
    let missing = (total - counts.values().sum::<u64>()) as f64;
    let maia_tail: f64 = maia
        .iter()
        .filter(|(played, _)| !counts.contains_key(*played))
        .map(|(_, p)| p)
        .sum();
    maia.into_iter()
        .map(|(played, prior)| {
            let count = match counts.get(&played) {
                Some(count) => *count as f64,
                None if maia_tail > 0.0 => missing * prior / maia_tail,
                None => 0.0,
            };
            (played, (count + PRIOR * prior) / (total as f64 + PRIOR))
        })
        .collect()
}

#[derive(Default)]
struct Estimate {
    covered: f64,
    unresolved: f64,
    gaps: BTreeMap<(PositionKey, String), f64>,
}

impl Estimate {
    fn suggestions(self, target_percent: u8) -> Vec<GapMass> {
        let mut shortfall = f64::from(target_percent) / 100.0 - self.covered;
        let mut ranked: Vec<_> = self
            .gaps
            .into_iter()
            .map(|((position, played), p)| (position, played, p))
            .collect();
        ranked.sort_by(|a, b| {
            b.2.total_cmp(&a.2)
                .then_with(|| (&a.0, &a.1).cmp(&(&b.0, &b.1)))
        });
        ranked
            .into_iter()
            .map(|(position, played, p)| (position, played, p.min(1.0)))
            .take_while(|(_, _, p)| {
                let needed = shortfall > EPSILON;
                shortfall -= p;
                needed
            })
            .collect()
    }
}

fn estimate(
    book: &AuthoredRepertoire,
    color: LearnerColor,
    prefix: &[String],
    through_move: u8,
    mut evidence: impl FnMut(&PositionKey, &[String]) -> Distribution,
) -> Result<Estimate, AppError> {
    let mut result = Estimate::default();
    let (start, first) = book
        .coverage_frontier(prefix, color)
        .map_err(|error| domain("invalid_scan", error))?;
    let through = u16::from(through_move);
    if !prefix.is_empty() && through < first {
        return Err(AppError::new(
            "invalid_scan",
            format!("Choose a depth of at least move {first} for this starting line."),
        ));
    }
    let mut frontier = BTreeMap::from([(start, 1.0_f64)]);
    let mut probabilities = BTreeMap::new();
    let mut responses = BTreeMap::new();
    for _ in first..=through {
        let mut current: Vec<_> = std::mem::take(&mut frontier).into_iter().collect();
        current.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (position, mass) in current {
            let next =
                legal_successors(&position).map_err(|error| domain("invalid_repertoire", error))?;
            if next.is_empty() {
                result.covered += mass;
                continue;
            }
            let distribution = probabilities
                .entry(position.clone())
                .or_insert_with(|| evidence(&position, &next.keys().cloned().collect::<Vec<_>>()));
            let mut attributed = 0.0;
            for (played, chance) in distribution {
                let path = mass * *chance;
                if path <= 0.0 {
                    continue;
                }
                let Some(after) = next.get(played) else {
                    continue;
                };
                attributed += path;
                let response = match responses.entry(after.clone()) {
                    std::collections::btree_map::Entry::Occupied(entry) => *entry.get(),
                    std::collections::btree_map::Entry::Vacant(entry) => *entry.insert(
                        book.learner_response(after, color)
                            .map_err(|error| domain("invalid_repertoire", error))?,
                    ),
                };
                match response {
                    LearnerResponse::Prepared(next) => {
                        *frontier.entry(next.clone()).or_default() += path
                    }
                    LearnerResponse::Terminal => result.covered += path,
                    LearnerResponse::Missing => {
                        *result
                            .gaps
                            .entry((position.clone(), played.clone()))
                            .or_default() += path;
                    }
                }
            }
            result.unresolved += (mass - attributed).max(0.0);
        }
    }
    result.covered += frontier.into_values().sum::<f64>();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(got: f64, expected: f64) {
        assert!((got - expected).abs() < EPSILON, "{got} != {expected}");
    }

    fn estimate_with(
        pgn: &str,
        color: LearnerColor,
        depth: u8,
        evidence: &[&[(&str, f64)]],
    ) -> Estimate {
        let book = AuthoredRepertoire::parse(pgn).unwrap();
        let mut by_position = evidence.iter();
        estimate(&book, color, &[], depth, |_, _| {
            by_position
                .next()
                .unwrap_or(&&[][..])
                .iter()
                .map(|(uci, mass)| (uci.to_string(), *mass))
                .collect()
        })
        .unwrap()
    }

    #[test]
    fn black_conditioned_scan_samples_only_the_given_position_through_absolute_move_two() {
        let book = AuthoredRepertoire::parse("1.e4 c6 2.d4 d5 *\n\n1.d4 d5 2.c4 c6 *").unwrap();
        let color = LearnerColor::Black;
        let line = vec!["e2e4".into(), "c7c6".into()];
        let given = book.coverage_start(&line, color).unwrap().position;
        let run = |d4| {
            let mut lookups = 0;
            let estimate = estimate(&book, color, &line, 2, |position, _| {
                assert_eq!(position, &given);
                lookups += 1;
                BTreeMap::from([("d2d4".into(), d4), ("g1f3".into(), 0.25)])
            })
            .unwrap();
            assert_eq!(lookups, 1);
            estimate
        };
        let first = run(0.6);
        approx(first.covered, 0.6);
        approx(first.unresolved, 0.15);
        assert_eq!(
            first.suggestions(90),
            vec![(given.clone(), "g1f3".into(), 0.25)]
        );
        approx(run(0.4).covered, 0.4);
        let before_black_reply = &line[..1];
        assert!(estimate(&book, color, before_black_reply, 1, |_, _| unreachable!()).is_err());
        let after_forced_reply = estimate(&book, color, before_black_reply, 2, |position, _| {
            assert_eq!(position, &given);
            BTreeMap::from([("d2d4".into(), 1.0)])
        })
        .unwrap();
        approx(after_forced_reply.covered, 1.0);
    }

    #[test]
    fn white_ranked_first_gaps_account_for_target_shortfall() {
        let result = estimate_with(
            "1.e4 e5 2.Nf3 *",
            LearnerColor::White,
            2,
            &[&[("e7e5", 0.60), ("c7c5", 0.25), ("d7d5", 0.15)]],
        );
        approx(result.covered, 0.60);
        let gaps = result.suggestions(80);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].1, "c7c5");
        approx(gaps[0].2, 0.25);
    }

    #[test]
    fn transposed_position_with_saved_response_counts_under_both_paths() {
        let result = estimate_with(
            "1.Nf3 Nf6 (1...d5 2.g3) 2.g3 d5 3.Bg2 *",
            LearnerColor::White,
            3,
            &[
                &[("g8f6", 0.6), ("d7d5", 0.4)],
                &[("d7d5", 1.0)],
                &[("g8f6", 1.0)],
            ],
        );
        approx(result.covered, 1.0);
        assert!(result.suggestions(100).is_empty());
    }

    #[test]
    fn profile_blends_counts_and_unlisted_mass() {
        let counts = (300, BTreeMap::from([("a".into(), 150), ("b".into(), 90)]));
        let maia = BTreeMap::from([("a".into(), 0.2), ("b".into(), 0.5), ("c".into(), 0.3)]);
        let distribution = blend(Some(counts.clone()), Some(maia));
        approx(distribution["a"], 0.425);
        approx(distribution["b"], 0.35);
        approx(distribution["c"], 0.225);
        approx(blend(Some(counts), None).values().sum(), 0.8);
    }

    #[test]
    fn black_first_move_can_be_missing_and_white_short_line_can_be_extended() {
        let black = estimate_with(
            "1.e4 c5 *",
            LearnerColor::Black,
            1,
            &[&[("e2e4", 0.7), ("d2d4", 0.2)]],
        );
        approx(black.covered, 0.7);
        approx(black.unresolved, 0.1);
        assert_eq!(black.suggestions(95)[0].1, "d2d4");

        let white = estimate_with(
            "1.e4 e5 2.Nf3 *",
            LearnerColor::White,
            3,
            &[&[("e7e5", 1.0)], &[("b8c6", 0.8), ("g8f6", 0.2)]],
        );
        approx(white.covered, 0.0);
        let gaps = white.suggestions(75);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].1, "b8c6");
        approx(gaps[0].2, 0.8);
    }
}
