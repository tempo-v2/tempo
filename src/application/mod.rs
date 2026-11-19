//! Application operations, each in the file named for the public method.
//!
//! These child modules can access Tempo's private fields. No field needs to be
//! exposed publicly merely to organize an operation into its own source file.
mod abandon;
mod again;
mod backfill_captures;
mod content_preview;
mod current_attempt;

mod daily;
mod export;
mod game_backfill;
mod game_detection;
mod game_evidence;
mod game_model;
mod handle;
mod import_pgn;
mod ingest_games;
mod next_exercise;
mod optional_tactics;
mod queue_source;
mod receipts;
mod repertoires;
mod study_game_context;
mod study_analysis_context;

mod restore;
mod resume;
mod study_progress;
mod submit;
pub use study_progress::{ProgressStamp, StudyProgress};
mod tactic_capture;
mod tactic_catalog_facts;
mod tactic_preferences;
mod tactics_collection;
pub use tactics_collection::{TacticsRead, TacticsReadReply};

mod teach;

pub use daily::IntroductionLimits;
pub use tactic_preferences::{TacticThemes, tactic_themes};

use crate::{AppError, Components, app_error::domain};
use tempo_queue::OrderEvent;
use tempo_store::{Commit, MemoryStore, Snapshot, Store, StudyScope, StudyState, Write};
use tempo_types::TargetId;

/// Coordinate domain decisions and commit the resulting operation atomically.
///
/// Construct one with a store and [`Components`], or use [`Self::demo`] for the
/// wired fixture. Call methods with intent values; the queue selects content,
/// Study advances attempts, the grader decides correctness, Learning interprets
/// assessments, and Store accepts the assembled transaction.
///
/// ```
/// use tempo::Tempo;
/// let mut app = Tempo::demo()?;
/// let issued = app.next_exercise()?.expect("demo has an eligible exercise");
/// let resumed = app.resume(&issued.attempt.id)?;
/// assert_eq!(resumed.attempt, issued.attempt);
/// # Ok::<(), tempo::AppError>(())
/// ```
pub struct Tempo {
    store: Box<dyn Store>,
    components: Components,
    remaining_block: Vec<TargetId>,
    order_history: Vec<OrderEvent>,
    daily: Option<daily::DailyStudy>,
    game_books: Option<std::sync::Arc<game_evidence::GameBooks>>,
}
impl Tempo {
    /// Assemble the application after checking stored policy and learning compatibility.
    pub fn new(store: Box<dyn Store>, components: Components) -> Result<Self, AppError> {
        let snapshot = store.study_state(StudyScope {
            catalog: true,
            ..StudyScope::default()
        })?;
        if snapshot.policy != components.learning.settings() {
            return Err(AppError::new(
                "policy_mismatch",
                "stored learning policy differs from selected implementation",
            ));
        }
        let (_, learning) = snapshot.complete_catalog()?;
        for state in learning.values() {
            components
                .learning
                .validate_state(state)
                .map_err(|e| domain("invalid_learning", e))?;
        }
        Ok(Self {
            store,
            components,
            remaining_block: vec![],
            order_history: vec![],
            daily: None,
            game_books: None,
        })
    }
    /// Create the reproducible in-memory repertoire/tactic demonstration.
    pub fn demo() -> Result<Self, AppError> {
        let components = Components::fixtures();
        let store = MemoryStore::new(
            "demo-dataset".into(),
            tempo_chess::demo_catalog(),
            components.learning.settings(),
        )?;
        Self::new(Box::new(store), components)
    }
    /// Read a consistent store snapshot for application inspection or adapters.
    /// This internal-data shape is not the UI view contract.
    pub fn snapshot(&self) -> Result<Snapshot, AppError> {
        self.store.snapshot().map_err(Into::into)
    }
    fn commit_study(
        &mut self,
        snapshot: &StudyState,
        writes: Vec<Write>,
        scope: StudyScope<'_>,
    ) -> Result<(), AppError> {
        self.store.commit_study(
            Commit {
                expected_revision: snapshot.revision,
                expected_dataset: snapshot.dataset.clone(),
                writes,
            },
            scope,
        )?;
        Ok(())
    }

    fn commit(
        &mut self,
        dataset: &tempo_types::DatasetId,
        revision: tempo_types::Revision,
        writes: Vec<Write>,
    ) -> Result<(), AppError> {
        self.store.commit(Commit {
            expected_revision: revision,
            expected_dataset: dataset.clone(),
            writes,
        })?;
        Ok(())
    }
}
