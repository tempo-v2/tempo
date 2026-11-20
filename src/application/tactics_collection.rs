//! Read the current tactics collection with the study calendar and disclosure rules.
use super::Tempo;
use crate::{AppError, app_error::domain};
use serde::{Deserialize, Serialize};
use tempo_chess::{ContentStatus, LearnerColor, RevealedLine, project_position, revealed_line};
use tempo_store::{StudyScope, TacticFilter, TacticPage, TacticPageQuery};
use tempo_types::{DatasetId, TargetId};

#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum TacticsRead {
    List {
        #[serde(default)]
        search: String,
        #[serde(default)]
        filter: TacticFilter,
        after: Option<TargetId>,
    },
    Detail {
        dataset_id: DatasetId,
        target: TargetId,
        #[serde(default)]
        show_solution: bool,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TacticsReadReply {
    TacticsPage {
        dataset_id: DatasetId,
        #[serde(flatten)]
        page: TacticPage,
    },
    TacticDetail(Box<TacticDetail>),
}

#[derive(Debug, Serialize)]
pub struct TacticDetail {
    pub label: String,
    pub provenance: Vec<String>,
    pub fen: String,
    pub learner_color: LearnerColor,
    pub availability: ContentStatus,
    pub solution_blocked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solution: Option<RevealedLine>,
}

impl Tempo {
    /// Read one bounded page or an addressed tactic. Inspection writes nothing.
    /// Active study takes precedence over a request to disclose the same target.
    pub fn read_tactics(&self, command: TacticsRead) -> Result<TacticsReadReply, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.tactics_collection.read_tactics");
        match command {
            TacticsRead::List {
                search,
                filter,
                after,
            } => {
                if search.len() > 240 || after.as_ref().is_some_and(|id| id.0.len() > 300) {
                    return Err(AppError::new(
                        "invalid_query",
                        "Use a shorter tactics search.",
                    ));
                }
                let state = self.store.study_state(StudyScope::default())?;
                let page = self.store.tactic_page(TacticPageQuery {
                    search: search.trim(),
                    after: after.as_ref(),
                    filter,
                    day: self.components.clock.now().day,
                })?;
                Ok(TacticsReadReply::TacticsPage {
                    dataset_id: state.dataset,
                    page,
                })
            }
            TacticsRead::Detail {
                dataset_id,
                target,
                show_solution,
            } => {
                let state = self.store.study_state(StudyScope {
                    target: Some(&target),
                    ..StudyScope::default()
                })?;
                if state.dataset != dataset_id {
                    return Err(AppError::new(
                        "stale_dataset",
                        "The dataset changed. Reopen the tactics list.",
                    ));
                }
                let (item, _) = state
                    .target(&target)?
                    .filter(|(item, _)| !item.definition.is_repertoire())
                    .ok_or_else(|| {
                        AppError::new("unknown_tactic", "This tactic is unavailable.")
                    })?;
                let blocked = state
                    .active_attempt
                    .as_ref()
                    .and_then(|id| state.attempts.get(id))
                    .is_some_and(|record| record.attempt.target == target);
                let board =
                    project_position(&item.definition, &tempo_chess::ChessState::at(0), false)
                        .map_err(|error| domain("invalid_content", error))?;
                let solution = (show_solution && !blocked)
                    .then(|| revealed_line(&item.definition, item.definition.decisions.len()))
                    .transpose()
                    .map_err(|error| domain("invalid_content", error))?;
                Ok(TacticsReadReply::TacticDetail(Box::new(TacticDetail {
                    label: item.label.clone(),
                    provenance: item.provenance.clone(),
                    fen: board.display_fen,
                    learner_color: board.learner_color,
                    availability: item.status,
                    solution_blocked: blocked,
                    solution,
                })))
            }
        }
    }
}
