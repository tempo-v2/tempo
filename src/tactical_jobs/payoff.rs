//! Upgrade retained generated definitions while the normal analysis window is open.
use crate::tactical_engine::PayoffEngine;
use crate::{
    AppError,
    application::tactical_analysis::{Continuations, TacticalDetails, continuation_roots},
    game_detection::DetectionCompletion,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tempo_chess::{ContentItem, DetectionControl, DetectionStop, EvaluationFailure};
use tempo_store::SqliteStore;
use tempo_types::DatasetId;

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
    dataset: DatasetId,
    item: ContentItem,
    cancel: Arc<AtomicBool>,
    worker: JoinHandle<Result<Vec<String>, EvaluationFailure>>,
}

struct NewPuzzleSearch {
    cancel: Arc<AtomicBool>,
    worker: JoinHandle<Continuations>,
}

struct PendingRecognition {
    dataset: DatasetId,
    id: i64,
    evidence: String,
    cancel: Arc<AtomicBool>,
    worker: JoinHandle<Option<tempo_chess::TacticRecognition>>,
}

pub(super) struct Payoffs {
    engine: PayoffEngine,
    dataset: Option<DatasetId>,
    after: String,
    next: Instant,
    pending: Option<Pending>,
    new_puzzle: Option<NewPuzzleSearch>,
    recognition: Option<PendingRecognition>,
    recognition_before: i64,
    recognition_next: Instant,
}

impl Payoffs {
    pub(super) fn new(engine: PayoffEngine) -> Self {
        Self {
            engine,
            dataset: None,
            after: String::new(),
            next: Instant::now(),
            pending: None,
            new_puzzle: None,
            recognition: None,
            recognition_before: i64::MAX,
            recognition_next: Instant::now(),
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
                            .map(|line| ((index, answer.uci), line))
                    })
                    .collect()
            }),
        });
    }

    // The completed detection stays available even if this optional search
    // stops, times out, or fails. Pending denotes only continuation work.
    pub(super) fn new_puzzle_lines(&mut self) -> Option<Continuations> {
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
        if let Some(job) = &self.recognition {
            job.cancel.store(true, Ordering::Relaxed);
        }
        if let Some(job) = &self.pending {
            job.cancel.store(true, Ordering::Relaxed)
        }
    }
    pub(super) fn advance_retained(
        &mut self,
        store: &SqliteStore,
        dataset: &DatasetId,
        report_version: &str,
    ) -> Result<bool, AppError> {
        debug_assert!(self.new_puzzle.is_none());
        if self.dataset.as_ref() != Some(dataset) {
            self.cancel();
            self.after.clear();
            self.recognition_before = i64::MAX;
            self.recognition_next = Instant::now();
            self.dataset = Some(dataset.clone());
        }
        if self
            .recognition
            .as_ref()
            .is_some_and(|job| !job.worker.is_finished())
        {
            return Ok(true);
        }
        if let Some(job) = self.recognition.take() {
            if let Ok(Some(result)) = job.worker.join()
                && &job.dataset == dataset
            {
                store.publish_tactic_recognition(dataset, job.id, &job.evidence, &result)?;
            }
            return Ok(false);
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|j| !j.worker.is_finished())
        {
            return Ok(true);
        }
        if let Some(job) = self.pending.take() {
            let result = job
                .worker
                .join()
                .map_err(|_| AppError::new("analysis_failed", "Puzzle analysis worker stopped."))?;
            if &job.dataset == dataset {
                if let Ok(pv) = result {
                    let verdict = tempo_chess::requalify_game_tactic(&job.item, &pv)
                        .map_err(|e| AppError::new("invalid_tactic", e.to_string()))?;
                    store.publish_game_tactic_qualification(dataset, &job.item, verdict)?;
                }
            }
            // Release this dispatch turn to game detection before starting
            // another retained search, even when the retained page has more.
            return Ok(false);
        }
        if Instant::now() < self.next {
            return self.start_recognition(store, dataset, report_version);
        }
        let page = store.pending_game_tactic_qualifications(dataset, &self.after)?;
        if page.is_empty() {
            self.after.clear();
            self.next = Instant::now() + Duration::from_secs(60);
            return self.start_recognition(store, dataset, report_version);
        }
        for item in page {
            self.after.clone_from(&item.target.0);
            if store.game_tactic_qualification_held(dataset, &item)? {
                continue;
            }
            let decision = item.definition.decisions[0].clone();
            let engine = self.engine.clone();
            let cancel = Arc::new(AtomicBool::new(false));
            let control = Cancel(cancel.clone());
            self.pending = Some(Pending {
                dataset: dataset.clone(),
                item,
                cancel,
                worker: std::thread::spawn(move || {
                    engine.variation(&decision.fen, &decision.expected, &control)
                }),
            });
            break;
        }
        Ok(self.pending.is_some())
    }
    fn start_recognition(
        &mut self,
        store: &SqliteStore,
        dataset: &DatasetId,
        report_version: &str,
    ) -> Result<bool, AppError> {
        if Instant::now() < self.recognition_next {
            return Ok(false);
        }
        let page =
            store.pending_tactic_recognition(dataset, report_version, self.recognition_before)?;
        let Some((id, evidence)) = page.into_iter().next() else {
            self.recognition_before = i64::MAX;
            self.recognition_next = Instant::now() + Duration::from_secs(60);
            return Ok(false);
        };
        self.recognition_before = id;
        let detail: TacticalDetails = serde_json::from_str(&evidence)
            .map_err(|e| AppError::new("invalid_tactical_history", e.to_string()))?;
        let cancel = Arc::new(AtomicBool::new(false));
        let control = Cancel(cancel.clone());
        let engine = self.engine.clone();
        self.recognition = Some(PendingRecognition {
            dataset: dataset.clone(),
            id,
            evidence,
            cancel,
            worker: std::thread::spawn(move || {
                let mut lines = std::collections::BTreeMap::new();
                for root in tempo_chess::recognition_roots(&detail.assessment) {
                    let answer = tempo_chess::ChessAnswer::from(root.as_str());
                    if let Ok(line) = engine.variation(&detail.fen, &answer, &control) {
                        lines.insert(root, line);
                    }
                }
                tempo_chess::recognize_game_tactics(&detail.assessment, &detail.fen, |root| {
                    lines.get(&root.uci).map(Vec::as_slice)
                })
            }),
        });
        Ok(true)
    }
}

impl Drop for Payoffs {
    fn drop(&mut self) {
        self.cancel();
        if let Some(job) = self.new_puzzle.take() {
            let _ = job.worker.join();
        }
        if let Some(recognition) = self.recognition.take() {
            let _ = recognition.worker.join();
        }
        if let Some(pending) = self.pending.take() {
            let _ = pending.worker.join();
        }
    }
}
