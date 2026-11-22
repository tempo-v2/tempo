//! Application operations, each in the file named for the public method.
//!
//! These child modules can access Tempo's private fields. No field needs to be
//! exposed publicly merely to organize an operation into its own source file.
mod abandon;
mod content_preview;
mod coverage_settings;
pub(crate) use coverage_settings::{CoverageLineSave, PreviousCoverageLineRequest};
mod current_attempt;

mod daily;
mod export;
mod game_backfill;
mod game_evidence;
mod game_library;
pub use game_library::GameLibraryRead;
mod game_model;
mod game_review;
pub use game_review::{GameReviewLine, GameReviewRead};
mod handle;
mod import_pgn;
mod next_exercise;
mod optional_tactics;
mod receipts;
mod repertoires;
mod study_analysis_context;
mod study_card_details;
pub use study_analysis_context::StudyAnalysisLine;
mod study_game_context;

mod restore;
mod resume;
mod saved_reviews;
pub use saved_reviews::{SaveStudyReview, SavedStudyReviewCommand};
mod study_progress;
mod submit;
pub use study_progress::{ProgressStamp, StudyProgress};
mod tactic_capture;
pub(crate) mod tactic_catalog_facts;
mod tactic_preferences;
mod tactics_collection;
pub use tactics_collection::{TacticsRead, TacticsReadReply};

mod teach;

pub use daily::IntroductionLimits;
pub use tactic_preferences::{TacticThemes, tactic_themes};

use crate::{AppError, DailyComponents, app_error::domain};
use tempo_store::{Commit, Snapshot, Store, StudyScope, StudyState, Write};

/// Coordinate the daily Study application and commit accepted operations atomically.
///
/// Use [`Self::daily`] to assemble the application with its Store and runtime ports.
pub struct Tempo {
    store: Box<dyn Store>,
    components: DailyComponents,
    daily: Option<daily::DailyStudy>,
    game_books: Option<std::sync::Arc<game_evidence::GameBooks>>,
}
impl Tempo {
    fn assemble(
        store: Box<dyn Store>,
        components: DailyComponents,
        daily: daily::DailyStudy,
    ) -> Result<Self, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.mod.assemble");
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
            daily: Some(daily),
            game_books: None,
        })
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
