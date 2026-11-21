//! Save references to reached Study decisions and disclose them through Analysis.
use super::{Tempo, study_analysis_context::analysis_disclosure};
use crate::{
    AppError,
    app_error::{domain, study_input},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempo_store::{
    AcceptanceKind, ReviewCommandKey, ReviewPosition, ReviewWrite, TacticExclusionRead,
};
use tempo_study::AttemptStatus;
use tempo_types::{AttemptId, ContentVersion, DatasetId};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SaveStudyReview {
    dataset_id: DatasetId,
    attempt_id: AttemptId,
    content_version: ContentVersion,
    position: MarkPosition,
    submission_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum MarkPosition {
    Decision { index: usize },
    CompletedResponse,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum SavedStudyReviewCommand {
    List {
        dataset_id: DatasetId,
        before: Option<i64>,
    },
    Open {
        dataset_id: DatasetId,
        id: i64,
    },
    Check {
        dataset_id: DatasetId,
        id: i64,
    },
    CardDetails {
        dataset_id: DatasetId,
        id: i64,
    },
    Remove {
        dataset_id: DatasetId,
        id: i64,
        submission_id: String,
    },
    ExcludeTactic {
        dataset_id: DatasetId,
        id: i64,
        submission_id: String,
    },
}

enum SavedReviewRead {
    Analysis,
    Authorization,
    CardDetails,
}

impl Tempo {
    /// Marking authorizes saving an address only. It never discloses the move.
    pub fn save_study_review(&mut self, command: SaveStudyReview) -> Result<Value, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.saved_reviews.save_study_review");
        let fingerprint = command_fingerprint("save", &command)?;
        let key = command_key(&command.dataset_id, &command.submission_id, &fingerprint)?;
        let dataset = self.store.study_state(Default::default())?.dataset;
        if dataset != command.dataset_id {
            return Err(AppError::new("stale_dataset", "The Study dataset changed."));
        }
        let id = if let Some(id) = self.store.saved_review_receipt(&key)? {
            id
        } else {
            let record = self
                .store
                .stored_attempt(&command.attempt_id)?
                .filter(|record| record.attempt.dataset == dataset)
                .ok_or_else(|| {
                    AppError::new("unknown_attempt", "The issued exercise is unavailable.")
                })?;
            let attempt = &record.attempt;
            if attempt.content_version != command.content_version {
                return Err(AppError::new(
                    "stale_content",
                    "The issued content changed.",
                ));
            }
            let cursor = attempt.state.decision_index;
            let index = match command.position {
                MarkPosition::Decision { index } if index <= cursor => index,
                MarkPosition::CompletedResponse
                    if attempt.status == AttemptStatus::Complete && cursor > 0 =>
                {
                    cursor - 1
                }
                _ => {
                    return Err(AppError::new(
                        "invalid_decision",
                        "Study has not reached that decision.",
                    ));
                }
            };
            let issued = self
                .store
                .issued_item(&dataset, &attempt.target, attempt.content_version)?
                .ok_or_else(|| {
                    AppError::new("stale_content", "The issued content is unavailable.")
                })?;
            if index >= issued.definition.decisions.len() {
                return Err(AppError::new(
                    "invalid_decision",
                    "That Study decision is unavailable.",
                ));
            }
            self.store.write_saved_review(
                &key,
                ReviewWrite::Save(ReviewPosition {
                    attempt: &attempt.id,
                    target: &attempt.target,
                    version: attempt.content_version,
                    decision_index: index,
                    saved_at_ms: self.components.clock.now().unix_ms,
                }),
            )?
        };
        Ok(json!({"status": "saved", "id": id, "submission_id": command.submission_id}))
    }

    pub fn saved_study_reviews(
        &mut self,
        command: SavedStudyReviewCommand,
    ) -> Result<Value, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.saved_reviews.saved_study_reviews");
        match &command {
            SavedStudyReviewCommand::List { dataset_id, before } => {
                let mut rows = self.store.saved_review_page(dataset_id, *before)?;
                let has_more = rows.len() > 30;
                rows.truncate(30);
                let next = if has_more {
                    rows.last().map(|row| row.id)
                } else {
                    None
                };
                Ok(json!({"status": "saved_reviews", "rows": rows, "next": next}))
            }
            SavedStudyReviewCommand::Open { dataset_id, id } => {
                self.read_study_review(dataset_id, *id, SavedReviewRead::Analysis)
            }
            SavedStudyReviewCommand::Check { dataset_id, id } => {
                self.read_study_review(dataset_id, *id, SavedReviewRead::Authorization)
            }
            SavedStudyReviewCommand::CardDetails { dataset_id, id } => {
                self.read_study_review(dataset_id, *id, SavedReviewRead::CardDetails)
            }
            SavedStudyReviewCommand::Remove {
                dataset_id,
                id,
                submission_id,
            } => {
                let fingerprint = command_fingerprint("remove", &command)?;
                let key = command_key(dataset_id, submission_id, &fingerprint)?;
                let id = self
                    .store
                    .write_saved_review(&key, ReviewWrite::Remove(*id))?;
                Ok(json!({"status": "removed", "id": id, "submission_id": submission_id}))
            }
            SavedStudyReviewCommand::ExcludeTactic {
                dataset_id,
                id,
                submission_id,
            } => {
                let fingerprint = command_fingerprint("exclude_tactic", &command)?;
                let key = command_key(dataset_id, submission_id, &fingerprint)?;
                let id = match self.store.tactic_exclusion(&key, *id)? {
                    TacticExclusionRead::Accepted(id) => id,
                    TacticExclusionRead::Pending { revision, active } => {
                        let abandoned = active
                            .as_deref()
                            .map(|attempt| {
                                tempo_study::abandon(attempt, attempt.revision).map(Box::new)
                            })
                            .transpose()
                            .map_err(study_input)?;
                        self.store.write_saved_review(
                            &key,
                            ReviewWrite::ExcludeTactic {
                                id: *id,
                                expected_revision: revision,
                                abandoned,
                            },
                        )?
                    }
                };
                Ok(json!({"status": "excluded", "id": id, "submission_id": submission_id}))
            }
        }
    }

    fn read_study_review(
        &self,
        dataset: &DatasetId,
        id: i64,
        view: SavedReviewRead,
    ) -> Result<Value, AppError> {
        let context = self
            .store
            .saved_review_context(dataset, id)?
            .ok_or_else(|| AppError::new("unknown_review", "This saved mark is unavailable."))?;
        let source = &context.source;
        let legacy_acceptance = source.issued_revision.is_none()
            && source
                .acceptance
                .as_ref()
                .is_some_and(|acceptance| match acceptance.kind {
                    AcceptanceKind::WrongMove { decision_index, .. }
                    | AcceptanceKind::Revealed { decision_index } => {
                        decision_index == context.decision_index
                    }
                    _ => false,
                });
        let exact_reveal = source
            .revealed_decision
            .is_some_and(|proof| proof.decision_index == context.decision_index)
            || context.legacy_source_feedback
            || legacy_acceptance;
        let source_disclosed = source.mode.guided()
            || source.attempt.status == AttemptStatus::Complete
            || (source.attempt.status == AttemptStatus::Abandoned
                && analysis_disclosure(source).is_ok()
                && (context.decision_index < source.attempt.state.decision_index || exact_reveal));
        let later_disclosed = context.later_completed
            || context
                .later_abandoned_reveal
                .is_some_and(|proof| proof.decision_index >= context.decision_index);
        if context
            .active
            .as_ref()
            .is_some_and(|active| !active.mode.guided())
            || !(source_disclosed || later_disclosed || context.excluded_from_study)
        {
            return Ok(json!({"status": "study_required", "id": id}));
        }
        match view {
            SavedReviewRead::Authorization => Ok(json!({"status": "review_available", "id": id})),
            SavedReviewRead::Analysis => {
                let origin = self.project_saved_study_analysis(source, context.decision_index)?;
                Ok(json!({"status": "review_context", "id": id, "origin": origin}))
            }
            SavedReviewRead::CardDetails => {
                let details = self.project_saved_card_details(source)?;
                Ok(json!({"status": "card_details", "id": id, "details": details}))
            }
        }
    }
}

fn command_fingerprint(kind: &str, command: &impl Serialize) -> Result<String, AppError> {
    serde_json::to_string(&(kind, command)).map_err(|error| domain("codec_error", error))
}

fn command_key<'a>(
    dataset: &'a DatasetId,
    submission: &'a str,
    fingerprint: &'a str,
) -> Result<ReviewCommandKey<'a>, AppError> {
    if submission.is_empty() || submission.len() > 100 {
        return Err(AppError::new(
            "invalid_submission",
            "A review command ID is required.",
        ));
    }
    Ok(ReviewCommandKey {
        dataset,
        submission,
        fingerprint,
    })
}
