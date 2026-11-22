//! Run local probability inference outside the HTTP and Study application loop.
use crate::{AppError, Tempo, app_error::domain};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    },
    time::{Duration, Instant},
};
use tempo_chess::{AuthoredRepertoire, ContentKind, LearnerColor, PositionKey};
use tempo_store::{
    BookProjection, ModelInputStamp, ModelTargets, ProjectionBook, ProjectionScope,
    REACH_MODEL_KEY, SqliteStore, Store, StoreError,
};
use tempo_types::{DatasetId, Revision, TargetId};

use crate::maia::{Distribution, MODEL_KEY, Maia, PROFILE_LABEL, checked_distribution};
#[cfg(test)]
use tempo_chess::legal_model_moves;
const CACHE_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequest {
    pub dataset_id: DatasetId,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GapKind {
    #[default]
    Observed,
    Forecast,
}

/// Authoring authority established by the HTTP ingress, independent of wire data.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthoringSurface {
    Desktop,
    PhoneBuild,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GapRequest {
    pub dataset_id: DatasetId,
    pub repertoire_id: String,
    pub position: PositionKey,
    pub actual_move: String,
    pub document_revision: String,
    pub response: Option<String>,
    #[serde(default)]
    pub gap_kind: GapKind,
    pub report_id: Option<String>,
    pub base_revision: Option<Revision>,
}

struct ModelBook {
    color: LearnerColor,
    targets: BTreeMap<PositionKey, TargetId>,
}

pub(crate) fn model_input_stamp(
    authored: &AuthoredRepertoire,
    color: LearnerColor,
    targets: ModelTargets,
) -> ModelInputStamp {
    let routes: BTreeSet<Vec<_>> = authored
        .routes
        .iter()
        .map(|path| {
            path.iter()
                .map(|(before, played)| {
                    (before, played, &authored.positions[before].replies[played])
                })
                .collect()
        })
        .collect();
    let input = serde_json::to_vec(&(
        "tempo-model-input-v1",
        REACH_MODEL_KEY,
        color,
        routes,
        &targets,
    ))
    .expect("serializable repertoire model input");
    ModelInputStamp {
        key: blake3::hash(&input).to_hex().to_string(),
        targets,
    }
}

#[derive(Clone, Serialize)]
struct Status {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dataset_id: Option<DatasetId>,
    profile: &'static str,
    completed: usize,
    total: usize,
    projected: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            status: "idle",
            dataset_id: None,
            profile: PROFILE_LABEL,
            completed: 0,
            total: 0,
            projected: 0,
            error: None,
        }
    }
}

/// One publisher for cache-only projections and explicitly requested inference.
pub struct GameModel {
    wake: SyncSender<()>,
    jobs: Arc<Mutex<ModelJobs>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkIntent {
    CacheRefresh,
    Analyze,
}

impl WorkIntent {
    fn projection_scope(self) -> ProjectionScope {
        match self {
            Self::CacheRefresh => ProjectionScope::MissingOrPartial,
            Self::Analyze => ProjectionScope::AllVerified,
        }
    }
}

#[derive(Default)]
struct ModelJobs {
    explicit: Option<DatasetId>,
    cached: Option<CachedProjection>,
    active_explicit: Option<DatasetId>,
    status: Status,
}

struct CachedProjection {
    dataset: DatasetId,
    failures: usize,
    ready_at: Instant,
}

impl ModelJobs {
    fn request(&mut self, intent: WorkIntent, dataset: DatasetId, now: Instant) -> bool {
        match intent {
            WorkIntent::CacheRefresh => {
                self.cached = Some(CachedProjection {
                    dataset,
                    failures: 0,
                    ready_at: now,
                });
            }
            WorkIntent::Analyze => {
                if self.active_explicit.as_ref() == Some(&dataset)
                    || self.explicit.as_ref() == Some(&dataset)
                {
                    return false;
                }
                self.status = Status {
                    status: "running",
                    dataset_id: Some(dataset.clone()),
                    ..Status::default()
                };
                self.explicit = Some(dataset);
            }
        }
        true
    }

    fn next(&mut self, now: Instant) -> Option<(DatasetId, WorkIntent, usize)> {
        if let Some(dataset) = self.explicit.take() {
            self.active_explicit = Some(dataset.clone());
            return Some((dataset, WorkIntent::Analyze, 0));
        }
        if self.cached.as_ref()?.ready_at > now {
            return None;
        }
        self.cached
            .take()
            .map(|job| (job.dataset, WorkIntent::CacheRefresh, job.failures))
    }

    fn retry_cached(&mut self, dataset: DatasetId, failures: usize, now: Instant) -> bool {
        // A newer submission already covers the latest inputs and has its own retry budget.
        if self.cached.is_some() {
            return true;
        }
        let Some(delay) = CACHE_RETRY_DELAYS.get(failures) else {
            return false;
        };
        self.cached = Some(CachedProjection {
            dataset,
            failures: failures + 1,
            ready_at: now + *delay,
        });
        true
    }
}

impl GameModel {
    pub fn new(database: &Path) -> Result<Self, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("model.initialize");
        let (wake, queued) = sync_channel(1);
        let jobs = Arc::new(Mutex::new(ModelJobs::default()));
        let worker_jobs = Arc::clone(&jobs);
        let database = database.to_path_buf();
        std::thread::Builder::new()
            .name("tempo-maia".into())
            .spawn(move || {
                model_worker(&database, queued, &worker_jobs);
            })
            .map_err(|error| domain("model_unavailable", error))?;
        Ok(Self { wake, jobs })
    }

    pub fn status(&self, app: &Tempo) -> Result<Value, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("model.status");
        let dataset = app.game_dataset()?;
        let mut status = {
            let jobs = self.jobs.lock().expect("model jobs");
            if jobs.status.dataset_id.as_ref() == Some(&dataset) {
                json!(jobs.status)
            } else {
                json!(Status::default())
            }
        };
        if let Some(projection) = app.model_projection_status(&dataset)? {
            status["projection"] = json!(projection);
        }
        Ok(status)
    }

    pub fn start(&self, app: &Tempo, request: ModelRequest) -> Result<Value, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("model.dispatch");
        if app.game_dataset()? != request.dataset_id {
            return Err(AppError::new(
                "stale_dataset",
                "The active dataset changed.",
            ));
        }
        {
            let mut jobs = self.jobs.lock().expect("model jobs");
            if jobs.request(WorkIntent::Analyze, request.dataset_id, Instant::now()) {
                self.wake_worker()?;
            }
        }
        self.status(app)
    }

    pub fn project_cached(&self, dataset: DatasetId) -> Result<(), AppError> {
        let _diagnostic = crate::diagnostics::Span::new("model.schedule_cache");
        self.jobs.lock().expect("model jobs").request(
            WorkIntent::CacheRefresh,
            dataset,
            Instant::now(),
        );
        self.wake_worker()
    }

    fn wake_worker(&self) -> Result<(), AppError> {
        match self.wake.try_send(()) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => Ok(()),
            Err(error) => Err(domain("model_unavailable", error)),
        }
    }
}

fn model_worker(database: &Path, queued: Receiver<()>, jobs: &Mutex<ModelJobs>) {
    let directory = database.parent().unwrap_or(Path::new(".")).join("maia");
    loop {
        let (next, retry_delay) = {
            let mut state = jobs.lock().expect("model jobs");
            let now = Instant::now();
            let next = state.next(now);
            let retry_delay = state
                .cached
                .as_ref()
                .map(|job| job.ready_at.saturating_duration_since(now));
            (next, retry_delay)
        };
        if let Some((dataset, intent, failures)) = next {
            let mut trace = crate::diagnostics::Span::new(match intent {
                WorkIntent::Analyze => "model.inference_job",
                WorkIntent::CacheRefresh => "model.cached_projection",
            });
            let explicit = intent == WorkIntent::Analyze;
            let outcome = run(
                database,
                &directory,
                &dataset,
                intent,
                |completed, total| {
                    if explicit {
                        let mut state = jobs.lock().expect("model jobs");
                        if state.status.dataset_id.as_ref() == Some(&dataset) {
                            state.status.completed = completed;
                            state.status.total = total;
                        }
                    }
                },
            );
            trace.set_outcome(match &outcome {
                Ok((_, None)) => "complete",
                Ok((_, Some(_))) => "incomplete",
                Err(_) => "error",
            });
            if explicit {
                let mut state = jobs.lock().expect("model jobs");
                state.active_explicit = None;
                if state.status.dataset_id.as_ref() == Some(&dataset) {
                    let (projected, error) = outcome.unwrap_or_else(|error| (0, Some(error)));
                    state.status.projected = projected;
                    state.status.status = if error.is_some() {
                        "incomplete"
                    } else {
                        "complete"
                    };
                    state.status.error = error.map(|error| error.message);
                }
            } else if let Ok((_, Some(error))) | Err(error) = outcome {
                let retrying = jobs.lock().expect("model jobs").retry_cached(
                    dataset,
                    failures,
                    Instant::now(),
                );
                let disposition = if retrying {
                    "retry pending"
                } else {
                    "retries exhausted"
                };
                eprintln!("Tempo cached model projection ({disposition}): {error}");
            }
        } else if let Some(delay) = retry_delay {
            if queued.recv_timeout(delay) == Err(RecvTimeoutError::Disconnected) {
                break;
            }
        } else if queued.recv().is_err() {
            break;
        }
    }
}

struct PreparedBook {
    repertoire_id: String,
    input_key: String,
    book: ModelBook,
    tree: Prefix,
    jobs: BTreeSet<PositionKey>,
}

impl PreparedBook {
    fn new(source: ProjectionBook) -> Result<Self, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("model.initialize");
        let authored = AuthoredRepertoire::parse(&source.pgn)
            .map_err(|error| domain("invalid_repertoire", error))?;
        let mut targets = BTreeMap::new();
        let mut color = None;
        for (target, (_, definition)) in &source.targets {
            let ContentKind::Repertoire { position, .. } = &definition.kind else {
                return Err(AppError::new(
                    "invalid_repertoire",
                    "Modeled targets must belong to a repertoire.",
                ));
            };
            let Some(node) = authored.positions.get(position) else {
                return Err(AppError::new(
                    "invalid_repertoire",
                    "An active target is absent from its model document.",
                ));
            };
            if color
                .replace(node.side)
                .is_some_and(|side| side != node.side)
            {
                return Err(AppError::new(
                    "invalid_repertoire",
                    "A model book has more than one learner side.",
                ));
            }
            targets.insert(position.clone(), target.clone());
        }
        if let Some(color) = color
            && model_input_stamp(&authored, color, source.targets).key != source.input_key
        {
            return Err(AppError::new(
                "stale_repertoire",
                "The verified model input no longer matches its document.",
            ));
        }
        let jobs = authored
            .positions
            .iter()
            .filter(|(_, node)| {
                !node.replies.is_empty() && color.is_some_and(|side| node.side != side)
            })
            .map(|(position, _)| position.clone())
            .collect();
        Ok(Self {
            repertoire_id: source.repertoire_id,
            input_key: source.input_key,
            book: ModelBook {
                color: color.unwrap_or(LearnerColor::White),
                targets,
            },
            tree: Prefix::from_book(authored),
            jobs,
        })
    }
}

#[derive(Default)]
struct Prefix {
    position: Option<(PositionKey, LearnerColor)>,
    replies: BTreeMap<String, Prefix>,
}

impl Prefix {
    fn from_book(book: AuthoredRepertoire) -> Self {
        let mut root = Self::default();
        for path in book.routes {
            let mut node = &mut root;
            for (position, played) in path {
                let before = &book.positions[&position];
                let after = &before.replies[&played];
                node.position = Some((position, before.side));
                node = node.replies.entry(played).or_default();
                node.position = Some((after.clone(), book.positions[after].side));
            }
        }
        root
    }

    fn project(
        &self,
        book: &ModelBook,
        probability: Option<f64>,
        cache: &BTreeMap<PositionKey, Distribution>,
        path_hits: &mut BTreeSet<TargetId>,
        reach: &mut BTreeMap<TargetId, Option<f64>>,
    ) {
        let Some((position, side)) = &self.position else {
            return;
        };
        let first_hit = book
            .targets
            .get(position)
            .filter(|target| path_hits.insert((*target).clone()));
        if let Some(target) = first_hit {
            let total = reach.entry(target.clone()).or_insert(Some(0.0));
            *total = total.zip(probability).map(|(sum, path)| sum + path);
        }
        let opponent = *side != book.color;
        for (played, next) in &self.replies {
            let child = if opponent {
                let local = cache
                    .get(position)
                    .and_then(|moves| moves.get(played))
                    .copied();
                match (probability, local) {
                    (Some(0.0), _) | (_, Some(0.0)) => Some(0.0),
                    (Some(path), Some(local)) => Some(path * local),
                    _ => None,
                }
            } else {
                probability
            };
            next.project(book, child, cache, path_hits, reach);
        }
        if let Some(target) = first_hit {
            path_hits.remove(target);
        }
    }
}

fn run(
    database: &Path,
    directory: &Path,
    dataset: &DatasetId,
    intent: WorkIntent,
    mut progress: impl FnMut(usize, usize),
) -> Result<(usize, Option<AppError>), AppError> {
    let _diagnostic = crate::diagnostics::Span::new("model.run");
    let mut store = SqliteStore::open(database)?;
    let input = store.projection_books(dataset, intent.projection_scope())?;
    if input.is_empty() && intent == WorkIntent::Analyze {
        return Err(AppError::new(
            "model_unavailable",
            "No verified repertoire documents are available.",
        ));
    }
    let mut books = Vec::new();
    let mut error = None;
    for source in input {
        match PreparedBook::new(source) {
            Ok(book) => books.push(book),
            Err(invalid) => {
                error.get_or_insert(invalid);
            }
        }
    }
    let jobs: BTreeSet<_> = books
        .iter()
        .flat_map(|book| book.jobs.iter().cloned())
        .collect();
    let mut total = jobs.len();
    let mut cache = BTreeMap::new();
    let mut missing = Vec::new();
    let mut invalid_cache = BTreeSet::new();
    for position in jobs {
        if let Some(distribution) = store.game_move_distribution(MODEL_KEY, &position.0)? {
            match checked_distribution(&position, distribution) {
                Ok(distribution) => {
                    cache.insert(position, distribution);
                }
                Err(invalid) => {
                    invalid_cache.insert(position);
                    error.get_or_insert(invalid);
                }
            }
        } else {
            missing.push(position);
        }
    }
    progress(cache.len(), total);
    if intent == WorkIntent::Analyze && !missing.is_empty() {
        let result = (|| {
            let mut maia = Maia::start(directory)?;
            for position in missing {
                let suspended = store.suspended_repertoires(dataset)?;
                if !books.iter().any(|book| {
                    book.jobs.contains(&position) && !suspended.contains(&book.repertoire_id)
                }) {
                    total -= 1;
                    progress(cache.len(), total);
                    continue;
                }
                let distribution = maia.infer(&position)?;
                store.save_game_move_distribution(MODEL_KEY, &position.0, &distribution)?;
                cache.insert(position, distribution);
                progress(cache.len(), total);
            }
            Ok::<_, AppError>(())
        })();
        if let Err(incomplete) = result {
            error.get_or_insert(incomplete);
        }
    }
    let mut projected = 0;
    for input in books {
        // Reject new reach from invalid evidence. Keep any already published
        // current result and continue projecting independent repertoires.
        if !input.jobs.is_disjoint(&invalid_cache) {
            continue;
        }
        let mut reach = BTreeMap::new();
        input.tree.project(
            &input.book,
            Some(1.0),
            &cache,
            &mut BTreeSet::new(),
            &mut reach,
        );
        let complete = input
            .jobs
            .iter()
            .all(|position| cache.contains_key(position));
        let reach: BTreeMap<_, _> = input
            .book
            .targets
            .values()
            .filter_map(|target| {
                let mass = match reach.get(target) {
                    Some(mass) => *mass,
                    None => complete.then_some(0.0),
                }?;
                Some((
                    target.clone(),
                    (mass.clamp(0.0, 1.0) * 1_000_000.0).round() as u32,
                ))
            })
            .collect();
        let publication = store.replace_game_model_reach(
            dataset,
            BookProjection {
                repertoire_id: &input.repertoire_id,
                input_key: &input.input_key,
                model: REACH_MODEL_KEY,
                complete,
                reach: &reach,
            },
        );
        match publication {
            Ok(()) => projected += reach.len(),
            Err(StoreError::Conflict(message)) => {
                error.get_or_insert(AppError::new("stale_repertoire", message));
            }
            Err(failed) => {
                error.get_or_insert(failed.into());
            }
        }
    }
    Ok((projected, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempo_chess::{ContentKind, compile_repertoire};

    fn full_distribution(position: &PositionKey, known: &[(&str, f64)]) -> Distribution {
        let moves = legal_model_moves(position).unwrap();
        let other = (1.0 - known.iter().map(|(_, mass)| mass).sum::<f64>())
            / (moves.len() - known.len()) as f64;
        moves
            .into_iter()
            .map(|played| {
                let mass = known
                    .iter()
                    .find(|(candidate, _)| *candidate == played)
                    .map_or(other, |(_, mass)| *mass);
                (played, mass)
            })
            .collect()
    }

    #[test]
    fn a_model_host_only_reports_run_status_for_the_current_dataset() {
        use crate::{DailyComponents, IntroductionLimits};
        use tempo_chess::Catalog;
        use tempo_learning::{DailyLearning, LearningPolicy};

        let dir = tempfile::tempdir().unwrap();
        let open = |name: &str| {
            let learning = DailyLearning::default();
            let store = SqliteStore::create(
                &dir.path().join(format!("{name}.sqlite")),
                DatasetId::from(name),
                Catalog::default(),
                learning.settings(),
            )
            .unwrap();
            let mut components = DailyComponents::fixtures();
            components.learning = Box::new(learning);
            Tempo::daily(
                Box::new(store),
                components,
                IntroductionLimits::default(),
                [42; 32],
                0,
            )
            .unwrap()
        };
        let old = open("old");
        let current = open("current");
        let (wake, _controlled) = sync_channel(1);
        let host = GameModel {
            wake,
            jobs: Arc::new(Mutex::new(ModelJobs::default())),
        };
        let requesting = |dataset| ModelRequest {
            dataset_id: DatasetId::from(dataset),
        };
        let old_reply = host.start(&old, requesting("old")).unwrap();
        assert_eq!(old_reply["status"], "running");
        assert_eq!(old_reply["dataset_id"], "old");
        {
            // Hold an old run in flight through the same host while the
            // application changes its active dataset.
            let mut jobs = host.jobs.lock().unwrap();
            assert_eq!(
                jobs.next(Instant::now()),
                Some((DatasetId::from("old"), WorkIntent::Analyze, 0))
            );
        }
        let clean = host.status(&current).unwrap();
        assert_eq!(clean["status"], "idle");
        assert!(clean.get("dataset_id").is_none());
        assert_eq!(clean["projection"]["dataset_id"], "current");
        assert!(host.start(&current, requesting("old")).is_err());
        let new_reply = host.start(&current, requesting("current")).unwrap();
        assert_eq!(new_reply["status"], "running");
        assert_eq!(new_reply["dataset_id"], "current");
    }

    #[test]
    fn failed_cache_work_backs_off_and_yields_to_explicit_and_newer_work() {
        let dataset = DatasetId::from("model-proof");
        let newer = DatasetId::from("replacement");
        let now = Instant::now();
        let mut jobs = ModelJobs::default();
        let mut failed_at = now;
        for (failures, delay) in CACHE_RETRY_DELAYS.into_iter().enumerate() {
            assert!(jobs.retry_cached(dataset.clone(), failures, failed_at));
            assert_eq!(jobs.next(failed_at + delay / 2), None);
            if failures == 0 {
                assert!(jobs.request(WorkIntent::Analyze, dataset.clone(), failed_at));
                assert_eq!(
                    jobs.next(failed_at),
                    Some((dataset.clone(), WorkIntent::Analyze, 0))
                );
            }
            failed_at += delay;
            assert_eq!(
                jobs.next(failed_at),
                Some((dataset.clone(), WorkIntent::CacheRefresh, failures + 1))
            );
        }
        assert!(!jobs.retry_cached(dataset.clone(), CACHE_RETRY_DELAYS.len(), failed_at));
        assert!(jobs.next(failed_at).is_none());

        assert!(jobs.request(WorkIntent::CacheRefresh, newer.clone(), failed_at));
        assert!(jobs.retry_cached(dataset, 1, failed_at));
        assert_eq!(
            jobs.next(failed_at),
            Some((newer, WorkIntent::CacheRefresh, 0))
        );
    }

    #[test]
    fn cache_only_projection_publishes_partial_and_terminal_reach_without_python() {
        use crate::{DailyComponents, IntroductionLimits};
        use tempo_chess::Catalog;
        use tempo_learning::{DailyLearning, LearningPolicy};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("projection.sqlite");
        let dataset = DatasetId::from("model-proof");
        let pgn = "1. Nf3 Nf6 (1... g6 2. g3 Nf6) 2. g3 g6 3. Bg2 *";
        let learning = DailyLearning::default();
        let store = SqliteStore::create(
            &path,
            dataset.clone(),
            Catalog::default(),
            learning.settings(),
        )
        .unwrap();
        let mut components = DailyComponents::fixtures();
        components.learning = Box::new(learning);
        let mut app = Tempo::daily(
            Box::new(store),
            components,
            IntroductionLimits::default(),
            [42; 32],
            0,
        )
        .unwrap();
        let mut command = json!({
            "command":"preview", "dataset_id":dataset, "pgn":pgn, "name":"Terminal", "learner_color":"white"
        });
        let preview = app.handle_repertoire(serde_json::from_value(command.clone()).unwrap());
        assert_eq!(preview["status"], "preview", "{preview}");
        command["command"] = json!("save");
        command["token"] = preview["token"].clone();
        let saved = app.handle_repertoire(serde_json::from_value(command).unwrap());
        assert_eq!(saved["status"], "repertoires", "{saved}");

        let authored = AuthoredRepertoire::parse(pgn).unwrap();
        let paths = &authored.routes;
        let endpoint = &paths[0][4].0;
        let mut store = SqliteStore::open(&path).unwrap();
        let target = app.snapshot().unwrap().catalog.items.into_values().find(|item| {
            matches!(&item.definition.kind, ContentKind::Repertoire { position, .. } if position == endpoint)
        }).unwrap().target;
        for (position, probabilities) in [
            (&paths[0][1].0, &[("g8f6", 0.4), ("g7g6", 0.3)][..]),
            (&paths[0][3].0, &[("g7g6", 0.5)][..]),
        ] {
            store
                .save_game_move_distribution(
                    MODEL_KEY,
                    &position.0,
                    &full_distribution(position, probabilities),
                )
                .unwrap();
        }
        let no_python = dir.path().join("missing-maia");
        let refresh = || {
            let (_, error) = run(
                &path,
                &no_python,
                &dataset,
                WorkIntent::CacheRefresh,
                |_, _| {},
            )
            .unwrap();
            assert!(error.is_none(), "{error:?}");
        };
        refresh();
        assert_eq!(
            store
                .game_model_projection_status(&dataset)
                .unwrap()
                .unwrap()
                .partial_current,
            1
        );
        assert!(
            !store
                .game_relevance(&dataset, REACH_MODEL_KEY)
                .unwrap()
                .contains_key(&target)
        );
        // An existing partial header is eligible even when the cache still cannot complete it.
        assert_eq!(
            store
                .projection_books(&dataset, ProjectionScope::MissingOrPartial)
                .unwrap()
                .len(),
            1
        );
        refresh();

        let alternate = &paths[1][3].0;
        store
            .save_game_move_distribution(
                MODEL_KEY,
                &alternate.0,
                &full_distribution(alternate, &[("g8f6", 0.6)]),
            )
            .unwrap();
        refresh();
        assert_eq!(
            store
                .game_model_projection_status(&dataset)
                .unwrap()
                .unwrap()
                .complete_current,
            1
        );
        assert_eq!(
            store.game_relevance(&dataset, REACH_MODEL_KEY).unwrap()[&target].modeled_millionths,
            Some(380_000)
        );
        assert!(
            store
                .projection_books(&dataset, ProjectionScope::MissingOrPartial)
                .unwrap()
                .is_empty()
        );
        // A retained map can decode correctly and still be invalid
        // evidence. It cannot replace a verified current result.
        let mut excess_mass = full_distribution(alternate, &[("g8f6", 1.0)]);
        let another = excess_mass
            .keys()
            .find(|key| *key != "g8f6")
            .unwrap()
            .clone();
        excess_mass.insert(another, 1.0);
        let excess_mass = serde_json::to_string(&excess_mass).unwrap();
        let output = std::process::Command::new("sqlite3")
            .arg(&path)
            .arg(format!(
                "UPDATE game_move_models SET distribution='{excess_mass}' WHERE model_key='{MODEL_KEY}' AND position='{}'",
                alternate.0
            ))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let (published, error) =
            run(&path, &no_python, &dataset, WorkIntent::Analyze, |_, _| {}).unwrap();
        assert_eq!(published, 0);
        assert_eq!(error.unwrap().code, "invalid_maia_reply");
        assert_eq!(
            store.game_relevance(&dataset, REACH_MODEL_KEY).unwrap()[&target].modeled_millionths,
            Some(380_000)
        );
    }

    #[test]
    fn modeled_reach_counts_first_arrival_and_keeps_missing_paths_unknown() {
        let pgn = "1. Nf3 Nf6 (1... g6 2. g3 Nf6) 2. g3 g6 3. Bg2 *";
        let color = LearnerColor::White;
        let catalog = compile_repertoire(pgn, "book", "Book", color, "test").unwrap();
        let targets: BTreeMap<_, _> = catalog
            .items
            .values()
            .filter_map(|item| match &item.definition.kind {
                ContentKind::Repertoire { position, .. } => {
                    Some((position.clone(), item.target.clone()))
                }
                _ => None,
            })
            .collect();
        let target = catalog
            .items
            .values()
            .find(|item| item.definition.decisions[0].expected.uci == "f1g2")
            .unwrap()
            .target
            .clone();
        let authored = AuthoredRepertoire::parse(pgn).unwrap();
        let paths = authored.routes.clone();
        let alternate = &paths[1][3];
        let endpoint = &authored.positions[&alternate.0].replies[&alternate.1];
        assert_eq!(endpoint, &paths[0][4].0);
        assert_eq!(targets.get(endpoint), Some(&target));
        assert!(authored.positions[endpoint].replies.contains_key("f1g2"));
        let root = Prefix::from_book(authored);
        let book = ModelBook { color, targets };
        let branch = paths[0][1].0.clone();
        let mut cache = BTreeMap::from([(
            branch,
            BTreeMap::from([("g8f6".into(), 0.4), ("g7g6".into(), 0.3)]),
        )]);
        cache.insert(
            paths[0][3].0.clone(),
            BTreeMap::from([("g7g6".into(), 0.5)]),
        );
        let mut reach = BTreeMap::new();
        root.project(&book, Some(1.0), &cache, &mut BTreeSet::new(), &mut reach);
        assert_eq!(reach[&target], None);
        let branch = &paths[0][1].0;
        cache.get_mut(branch).unwrap().insert("g7g6".into(), 0.0);
        reach.clear();
        root.project(&book, Some(1.0), &cache, &mut BTreeSet::new(), &mut reach);
        assert_eq!(reach[&target], Some(0.20));
        let alternate = paths[1][3].0.clone();
        let first = cache.remove(branch).unwrap();
        cache.insert(alternate.clone(), BTreeMap::from([("g8f6".into(), 0.0)]));
        // The branch distribution is absent, so the direct arrival is unknown.
        // Inspect the alternative subtree in isolation to see its determined zero.
        let alternative = &root.replies["g1f3"].replies["g7g6"];
        reach.clear();
        alternative.project(&book, None, &cache, &mut BTreeSet::new(), &mut reach);
        assert_eq!(reach[&target], Some(0.0));
        cache.insert(branch.clone(), first);
        cache.get_mut(branch).unwrap().insert("g7g6".into(), 0.3);
        cache.insert(alternate, BTreeMap::from([("g8f6".into(), 0.6)]));
        let mut reach = BTreeMap::new();
        root.project(&book, Some(1.0), &cache, &mut BTreeSet::new(), &mut reach);
        assert!((reach[&target].unwrap() - (0.4 * 0.5 + 0.3 * 0.6)).abs() < 1e-12);

        let continued =
            AuthoredRepertoire::parse("1. Nf3 Nf6 (1... g6 2. g3 Nf6 3. Bg2) 2. g3 g6 3. Bg2 *")
                .unwrap();
        let mut continued_reach = BTreeMap::new();
        Prefix::from_book(continued).project(
            &book,
            Some(1.0),
            &cache,
            &mut BTreeSet::new(),
            &mut continued_reach,
        );
        assert_eq!(continued_reach, reach);
    }
}
