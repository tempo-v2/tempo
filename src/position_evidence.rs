//! Independent, read-only evidence for the board currently under exploration.
//! One running and one latest waiting request per source bound interactive work.
use crate::{
    AppError,
    app_error::domain,
    explorer::{ExplorerClient, Ratings},
    game_evidence::check_id,
    maia::{MODEL_KEY, Maia, PROFILE_LABEL, checked_distribution},
    stockfish::stockfish,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tempo_chess::{
    LearnerColor, PositionCanonicalizer, PositionKey, StandardPositionCanonicalizer,
    legal_model_moves,
};
use tempo_store::{SqliteStore, Store};
use tempo_types::{DatasetId, Revision};
mod repertoires;
use repertoires::BookPositions;

#[derive(Clone, Copy, PartialOrd, Ord, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    Repertoire,
    Stockfish,
    Rated,
    Masters,
    Maia,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewVersion {
    revision: Revision,
    document_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRequest {
    request_id: String,
    fen: String,
    source: Source,
    dataset_id: Option<DatasetId>,
    repertoire_id: Option<String>,
    learner_color: Option<LearnerColor>,
    #[serde(default)]
    ratings: Ratings,
    #[serde(default)]
    pins: Vec<String>,
    candidate_count: Option<usize>,
    review_version: Option<ReviewVersion>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRead {
    request_id: String,
}

struct Job {
    request: EvidenceRequest,
    key: PositionKey,
    legal: Vec<String>,
}

#[derive(Default)]
struct Jobs {
    active: BTreeSet<Source>,
    waiting: BTreeMap<Source, Job>,
    replies: BTreeMap<String, Value>,
    finished: VecDeque<String>,
}
impl Jobs {
    fn finish(&mut self, id: String, reply: Value) {
        self.replies.insert(id.clone(), reply);
        self.finished.push_back(id);
        while self.finished.len() > 48 {
            if let Some(old) = self.finished.pop_front() {
                self.replies.remove(&old);
            }
        }
    }
}

pub struct PositionEvidence {
    jobs: Arc<Mutex<Jobs>>,
    services: Arc<Services>,
}
struct Services {
    database: PathBuf,
    directory: PathBuf,
    explorer: Arc<ExplorerClient>,
    books: Mutex<Option<BookPositions>>,
    maia: Mutex<Option<Maia>>,
}

impl PositionEvidence {
    pub fn new(database: &Path, explorer: Arc<ExplorerClient>) -> Self {
        Self {
            jobs: Default::default(),
            services: Arc::new(Services {
                database: database.to_owned(),
                directory: database.parent().unwrap_or(Path::new(".")).join("maia"),
                explorer,
                books: Default::default(),
                maia: Default::default(),
            }),
        }
    }
    pub fn read(&self, read: EvidenceRead) -> Result<Value, AppError> {
        self.jobs
            .lock()
            .expect("position evidence")
            .replies
            .get(&read.request_id)
            .cloned()
            .ok_or_else(|| {
                AppError::new(
                    "unknown_request",
                    "Request expired. Request this evidence again.",
                )
            })
    }
    pub fn start(&self, request: EvidenceRequest) -> Result<Value, AppError> {
        check_id(&request.request_id)?;
        let key = StandardPositionCanonicalizer
            .canonicalize(&request.fen)
            .map_err(|e| domain("invalid_position", e))?;
        let legal = legal_model_moves(&key).map_err(|e| domain("invalid_position", e))?;
        if request.pins.len() > 16 || request.pins.iter().any(|mv| !legal.contains(mv)) {
            return Err(AppError::new(
                "invalid_candidates",
                "Choose up to sixteen legal candidate moves.",
            ));
        }
        if request
            .candidate_count
            .is_some_and(|count| request.source != Source::Stockfish || !(1..=5).contains(&count))
        {
            return Err(AppError::new(
                "invalid_candidate_count",
                "Choose one to five Stockfish lines.",
            ));
        }
        if request.review_version.is_some()
            && (request.source != Source::Repertoire
                || request.repertoire_id.is_none()
                || request.dataset_id.is_none())
        {
            return Err(AppError::new(
                "invalid_review_scope",
                "Choose a game review and its repertoire.",
            ));
        }
        let id = request.request_id.clone();
        let source = request.source;
        let reply = json!({"status":"running","fen":request.fen,"request_id":id,"source":source});
        let mut jobs = self.jobs.lock().expect("position evidence");
        if jobs.replies.contains_key(&id) {
            return Err(AppError::new(
                "duplicate_request",
                "Use a fresh analysis request ID.",
            ));
        }
        jobs.replies.insert(id.clone(), reply.clone());
        if let Some(old) = jobs.waiting.insert(
            source,
            Job {
                request,
                key,
                legal,
            },
        ) {
            let r = old.request;
            jobs.finish(r.request_id.clone(),json!({"status":"error","code":"superseded","message":"A newer request replaced this lookup.","fen":r.fen,"request_id":r.request_id,"source":source}));
        }
        if jobs.active.insert(source) {
            let state = self.jobs.clone();
            let services = self.services.clone();
            if let Err(error) = std::thread::Builder::new()
                .name("tempo-position-evidence".into())
                .spawn(move || run_source(source, state, services))
            {
                jobs.active.remove(&source);
                jobs.waiting.remove(&source);
                jobs.replies.remove(&id);
                return Err(domain("evidence_unavailable", error));
            }
        }
        Ok(reply)
    }
}

fn run_source(source: Source, state: Arc<Mutex<Jobs>>, services: Arc<Services>) {
    loop {
        let job = {
            let mut state = state.lock().expect("position evidence");
            match state.waiting.remove(&source) {
                Some(job) => job,
                None => {
                    state.active.remove(&source);
                    break;
                }
            }
        };
        let result = services.read(&job);
        let r = job.request;
        let mut reply = json!({"fen":r.fen,"request_id":r.request_id,"source":source});
        match result {
            Ok(data) => {
                reply["status"] = json!("complete");
                reply["data"] = data;
            }
            Err(error) => {
                reply["status"] = json!("error");
                reply["code"] = json!(error.code);
                reply["message"] = json!(error.message);
            }
        }
        state
            .lock()
            .expect("position evidence")
            .finish(r.request_id, reply);
    }
}

impl Services {
    fn read(&self, job: &Job) -> Result<Value, AppError> {
        let r = &job.request;
        match r.source {
            Source::Stockfish => {
                let pins = r.pins.iter().map(String::as_str).collect::<Vec<_>>();
                let result = stockfish(
                    &self.directory,
                    &r.fen,
                    &job.legal,
                    &pins,
                    r.candidate_count.unwrap_or(3),
                )?;
                let complete = result.comparison.is_some() || pins.is_empty();
                Ok(
                    json!({"engine":result.engine,"comparison_complete":complete,"candidates":result.comparison.unwrap_or(result.candidates)}),
                )
            }
            Source::Rated | Source::Masters => {
                let rating = (r.source == Source::Rated).then_some(r.ratings);
                let stats = self
                    .explorer
                    .position(&r.fen, &job.legal, rating)
                    .map_err(|code| {
                        AppError::new(code, format!("Lichess lookup unavailable ({code})."))
                    })?;
                Ok(json!(stats))
            }
            Source::Maia => {
                let store = SqliteStore::open(&self.database)?;
                let distribution =
                    if let Some(cached) = store.game_move_distribution(MODEL_KEY, &job.key.0)? {
                        checked_distribution(&job.key, cached)?
                    } else if job.legal.is_empty() {
                        BTreeMap::new()
                    } else {
                        let mut worker = self.maia.lock().expect("interactive Maia");
                        if worker.is_none() {
                            *worker = Some(Maia::start(&self.directory)?);
                        }
                        match worker.as_mut().expect("initialized Maia").infer(&job.key) {
                            Ok(distribution) => distribution,
                            Err(error) => {
                                *worker = None;
                                return Err(error);
                            }
                        }
                    };
                Ok(
                    json!({"profile":PROFILE_LABEL,"terminal":job.legal.is_empty(),"probabilities":distribution}),
                )
            }
            Source::Repertoire => {
                let store = SqliteStore::open(&self.database)?;
                let marker = store.model_content_marker()?;
                if r.dataset_id
                    .as_ref()
                    .is_some_and(|dataset| dataset != &marker.0)
                {
                    return Err(AppError::new(
                        "stale_dataset",
                        "The active dataset changed.",
                    ));
                }
                let mut books = self.books.lock().expect("position repertoire snapshot");
                let game_review = r.review_version.is_some();
                if books.as_ref().is_none_or(|b| {
                    b.marker != marker
                        || b.scope != (r.repertoire_id.clone(), r.learner_color, game_review)
                }) {
                    *books = Some(BookPositions::load(
                        &store,
                        marker.clone(),
                        r.repertoire_id.as_deref(),
                        r.learner_color,
                        game_review,
                    )?);
                }
                let data = books.as_ref().expect("loaded book positions").compare(
                    &r.fen,
                    &job.key,
                    r.review_version.as_ref(),
                )?;
                if store.model_content_marker()? != marker {
                    return Err(AppError::new(
                        "stale_repertoire",
                        "The repertoire changed. Refresh the position.",
                    ));
                }
                Ok(data)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempo_chess::{Catalog, START_FEN};
    use tempo_learning::{DailyLearning, LearningPolicy};

    #[test]
    fn cached_maia_on_the_shared_board_rejects_bad_mass_without_running_inference() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("evidence.sqlite");
        let dataset = DatasetId::from("evidence-proof");
        let mut store = SqliteStore::create(
            &database,
            dataset,
            Catalog::default(),
            DailyLearning::default().settings(),
        )
        .unwrap();
        let make_job = |fen: &str| {
            let key = StandardPositionCanonicalizer.canonicalize(fen).unwrap();
            Job {
                legal: legal_model_moves(&key).unwrap(),
                key,
                request: EvidenceRequest {
                    request_id: "maia-fixture".into(),
                    fen: fen.into(),
                    source: Source::Maia,
                    dataset_id: None,
                    repertoire_id: None,
                    learner_color: None,
                    ratings: Ratings::default(),
                    pins: vec![],
                    candidate_count: None,
                    review_version: None,
                },
            }
        };
        let initial = make_job(START_FEN);
        let excess = initial
            .legal
            .iter()
            .map(|mv| {
                (
                    mv.clone(),
                    if ["e2e4", "d2d4"].contains(&mv.as_str()) {
                        1.0
                    } else {
                        0.0
                    },
                )
            })
            .collect();
        let query = make_job("rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1");
        let valid = query
            .legal
            .iter()
            .map(|mv| (mv.clone(), if *mv == "e7e5" { 1.0 } else { 0.0 }))
            .collect();
        store
            .save_game_move_distribution(MODEL_KEY, &initial.key.0, &excess)
            .unwrap();
        store
            .save_game_move_distribution(MODEL_KEY, &query.key.0, &valid)
            .unwrap();
        let host = PositionEvidence::new(&database, Arc::new(ExplorerClient::new(&database)));
        assert_eq!(
            host.services.read(&initial).unwrap_err().code,
            "invalid_maia_reply"
        );
        let response = host.services.read(&query).unwrap();
        assert_eq!(response["probabilities"], json!(valid));
        assert_eq!(response["terminal"], false);
        let terminal = host
            .services
            .read(&make_job("7k/6Q1/6K1/8/8/8/8/8 b - - 0 50"))
            .unwrap();
        assert_eq!(terminal["terminal"], true);
        assert_eq!(terminal["probabilities"], json!({}));
        assert!(host.services.maia.lock().unwrap().is_none());
        assert_eq!(
            store
                .game_move_distribution(MODEL_KEY, &initial.key.0)
                .unwrap(),
            Some(excess)
        );
    }
}
