//! Disclose one issued decision to the separate analysis page, after any required concession.
use super::Tempo;
use crate::{AppError, app_error::domain};
use serde_json::{Value, json};
use tempo_chess::ContentKind;
use tempo_store::{AcceptanceKind, ActivityMode, StudyScope};
use tempo_study::AttemptStatus;
use tempo_types::{AttemptId, ContentVersion, DatasetId, Grade, Revision};

impl Tempo {
    pub fn study_analysis_context(
        &self,
        dataset: &DatasetId,
        attempt_id: &AttemptId,
        revision: Revision,
        version: ContentVersion,
    ) -> Result<Value, AppError> {
        let state = self.store.study_state(StudyScope {
            attempt: Some(attempt_id),
            ..StudyScope::default()
        })?;
        let record = state.attempts.get(attempt_id).ok_or_else(|| {
            AppError::new("unknown_attempt", "The issued exercise is unavailable.")
        })?;
        let attempt = &record.attempt;
        if &state.dataset != dataset || &attempt.dataset != dataset {
            return Err(AppError::new("stale_dataset", "The Study dataset changed."));
        }
        if attempt.revision != revision
            || attempt.content_version != version
            || (attempt.status == AttemptStatus::Active
                && state.active_attempt.as_ref() != Some(attempt_id))
        {
            return Err(AppError::new(
                "stale_revision",
                "The issued exercise changed.",
            ));
        }
        if record.mode == ActivityMode::Test
            && !(attempt.status == AttemptStatus::Complete
                || (attempt.status == AttemptStatus::Abandoned
                    && attempt.terminal == Some(Grade::Again)))
        {
            return Err(AppError::new(
                "analysis_requires_concession",
                "End this review before opening analysis.",
            ));
        }
        if record.mode == ActivityMode::Practice
            && attempt.status == AttemptStatus::Active
            && attempt.terminal.is_none()
        {
            return Err(AppError::new(
                "analysis_requires_reveal",
                "Reveal this practice move before opening analysis.",
            ));
        }
        let issued = self
            .store
            .issued_item(dataset, &attempt.target, version)?
            .ok_or_else(|| AppError::new("stale_content", "The issued content is unavailable."))?;
        // On a completed exercise the cursor is one past the last decision.
        let index = attempt
            .state
            .decision_index
            .min(issued.definition.decisions.len().saturating_sub(1));
        let decision = issued
            .definition
            .decisions
            .get(index)
            .ok_or_else(|| AppError::new("invalid_content", "The exercise has no decision."))?;
        let repertoire = match &issued.definition.kind {
            ContentKind::Repertoire { repertoire, .. } => Some(repertoire.as_str()),
            ContentKind::Tactic { .. } => None,
        };
        let game = if record.mode == ActivityMode::Repair {
            self.study_game_context(dataset, attempt_id)?
        } else {
            Value::Null
        };
        let played = game["context"]["played_uci"].as_str().or_else(|| {
            match &record.acceptance.as_ref()?.kind {
                AcceptanceKind::WrongMove {
                    uci,
                    decision_index,
                } if *decision_index == index => Some(uci.as_str()),
                _ => None,
            }
        });
        Ok(json!({
            "fen": decision.fen, "color": issued.definition.learner_color().map_err(|e| domain("invalid_content", e))?,
            "datasetId": dataset, "repertoireId": repertoire, "name": record.exercise_name,
            "prescribedUci": decision.expected.uci, "playedUci": played,
            "attemptId": attempt_id, "revision": revision, "contentVersion": version,
        }))
    }
}
