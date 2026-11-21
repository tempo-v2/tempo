//! Upgrade retained generated definitions while the normal analysis window is open.
use crate::tactical_engine::PayoffEngine;
use crate::{
    AppError, application::tactical_analysis::continuation_roots,
    game_detection::DetectionCompletion,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use std::{
    collections::{BTreeMap, BTreeSet},
    thread::JoinHandle,
};
use tempo_chess::{
    ContentItem, DetectionControl, DetectionStop, EvaluationFailure, short_generated_decision,
};
use tempo_store::{SqliteStore, TacticPayoffPublication};
use tempo_types::DatasetId;
use tempo_types::{ContentVersion, TargetId};

struct Cancel(Arc<AtomicBool>);
impl DetectionControl for Cancel {
    fn check(&self) -> Result<(), DetectionStop> {
        if self.0.load(Ordering::Relaxed) {
            Err(DetectionStop::Cancelled)
        } else {
            Ok(())
        }
    }
}

struct Pending {
    id: i64,
    dataset: DatasetId,
    item: ContentItem,
    cancel: Arc<AtomicBool>,
    worker: JoinHandle<Result<Vec<String>, EvaluationFailure>>,
}

struct NewPuzzleSearch {
    cancel: Arc<AtomicBool>,
    worker: JoinHandle<BTreeMap<usize, Vec<String>>>,
}

pub(super) struct Payoffs {
    engine: PayoffEngine,
    dataset: Option<DatasetId>,
    before: i64,
    next: Instant,
    // A complete search that proves no extension needs no more engine time
    // for this version. A target held by an earlier request is revisited.
    checked: BTreeSet<(TargetId, ContentVersion)>,
    pending: Option<Pending>,
    new_puzzle: Option<NewPuzzleSearch>,
}

impl Payoffs {
    pub(super) fn new(engine: PayoffEngine) -> Self {
        Self {
            engine,
            dataset: None,
            before: i64::MAX,
            next: Instant::now(),
            checked: BTreeSet::new(),
            pending: None,
            new_puzzle: None,
        }
    }
    pub(super) fn start_new_puzzle(&mut self, result: &DetectionCompletion) {
        let roots = continuation_roots(result);
        if roots.is_empty() {
            return;
        }
        let engine = self.engine.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let control = Cancel(cancel.clone());
        self.new_puzzle = Some(NewPuzzleSearch {
            cancel,
            worker: std::thread::spawn(move || {
                roots
                    .into_iter()
                    .filter_map(|(index, fen, answer)| {
                        engine
                            .variation(&fen, &answer, &control)
                            .ok()
                            .map(|line| (index, line))
                    })
                    .collect()
            }),
        });
    }

    // The completed detection stays available even if this optional search
    // stops, times out, or fails. Pending denotes only continuation work.
    pub(super) fn new_puzzle_lines(&mut self) -> Option<BTreeMap<usize, Vec<String>>> {
        if self
            .new_puzzle
            .as_ref()
            .is_some_and(|job| !job.worker.is_finished())
        {
            return None;
        }
        Some(
            self.new_puzzle
                .take()
                .and_then(|job| job.worker.join().ok())
                .unwrap_or_default(),
        )
    }

    pub(super) fn cancel(&self) {
        if let Some(job) = &self.new_puzzle {
            job.cancel.store(true, Ordering::Relaxed);
        }
        if let Some(job) = &self.pending {
            job.cancel.store(true, Ordering::Relaxed)
        }
    }
    pub(super) fn poll(
        &mut self,
        store: &SqliteStore,
        dataset: &DatasetId,
        allowed: bool,
    ) -> Result<(), AppError> {
        if !allowed {
            self.cancel();
            return Ok(());
        }
        if self.new_puzzle.is_some() {
            return Ok(());
        }
        if self.dataset.as_ref() != Some(dataset) {
            self.cancel();
            self.checked.clear();
            self.before = i64::MAX;
            self.dataset = Some(dataset.clone());
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|j| !j.worker.is_finished())
        {
            return Ok(());
        }
        if let Some(job) = self.pending.take() {
            let result = job
                .worker
                .join()
                .map_err(|_| AppError::new("analysis_failed", "Puzzle analysis worker stopped."))?;
            if &job.dataset == dataset {
                match result {
                    Ok(pv) => {
                        let candidate = tempo_chess::extend_generated_tactic(&job.item, &pv)
                            .map_err(|e| AppError::new("invalid_tactic", e.to_string()))?;
                        let done = match candidate {
                            Some(extension) => {
                                matches!(
                                    store.publish_tactic_payoff(dataset, job.id, &extension)?,
                                    TacticPayoffPublication::Published
                                        | TacticPayoffPublication::ConflictingDefinition
                                )
                            }
                            None => true,
                        };
                        if done {
                            self.checked.insert((job.item.target, job.item.version));
                        }
                    }
                    Err(EvaluationFailure::Stopped(_)) => {
                        self.before = job.id + 1;
                    }
                    Err(_) => {} // retry on the next pass
                }
            }
        }
        if Instant::now() < self.next {
            return Ok(());
        }
        let page = store.game_tactic_candidates(dataset, self.before)?;
        if page.is_empty() {
            self.before = i64::MAX;
            self.next = Instant::now() + Duration::from_secs(60);
            return Ok(());
        }
        for (id, item) in page {
            self.before = id;
            let Some(decision) = short_generated_decision(&item) else {
                continue;
            };
            if self.checked.contains(&(item.target.clone(), item.version))
                || store.tactic_payoff_held(dataset, &item)?
            {
                continue;
            }
            let decision = decision.clone();
            let engine = self.engine.clone();
            let cancel = Arc::new(AtomicBool::new(false));
            let control = Cancel(cancel.clone());
            self.pending = Some(Pending {
                id,
                dataset: dataset.clone(),
                item,
                cancel,
                worker: std::thread::spawn(move || {
                    engine.variation(&decision.fen, &decision.expected, &control)
                }),
            });
            break;
        }
        Ok(())
    }
}

impl Drop for Payoffs {
    fn drop(&mut self) {
        self.cancel();
        if let Some(job) = self.new_puzzle.take() {
            let _ = job.worker.join();
        }
        if let Some(pending) = self.pending.take() {
            let _ = pending.worker.join();
        }
    }
}
