//! Authorize reads of an issued decision and, on request, its complete line.
use super::{Tempo, study_card_details::CardDisclosure};
use crate::{AppError, app_error::domain};
use serde::Serialize;
use serde_json::{Value, json};
use tempo_chess::{
    ContentItem, ContentKind, RevealedLine, first_matching_repertoire_line, revealed_line,
};
use tempo_store::{AcceptanceKind, ActivityMode, StoredAttempt, StudyScope};
use tempo_study::AttemptStatus;
use tempo_types::{AttemptId, ContentVersion, DatasetId, Grade, Revision};

/// A read-only export for an authorized Study visit. Missing authored context
/// can still provide the issued move. Lookup failures return an application error.
#[derive(Debug, Serialize)]
#[serde(tag = "scope", content = "line", rename_all = "snake_case")]
pub enum StudyAnalysisLine {
    FullLine(RevealedLine),
    IssuedMove(RevealedLine),
}

impl Tempo {
    pub fn study_analysis_context(
        &self,
        dataset: &DatasetId,
        attempt_id: &AttemptId,
        revision: Revision,
        version: ContentVersion,
    ) -> Result<Value, AppError> {
        let _diagnostic =
            crate::diagnostics::Span::new("app.study_analysis_context.study_analysis_context");
        let (record, issued) = self.authorized_analysis(dataset, attempt_id, revision, version)?;
        let attempt = &record.attempt;
        // On a completed exercise the cursor is one past the last decision.
        let index = attempt
            .state
            .decision_index
            .min(issued.definition.decisions.len().saturating_sub(1));
        self.project_study_analysis(&record, &issued, index)
    }

    pub(super) fn project_saved_study_analysis(
        &self,
        record: &StoredAttempt,
        index: usize,
    ) -> Result<Value, AppError> {
        let _diagnostic = crate::diagnostics::Span::new(
            "app.study_analysis_context.project_saved_study_analysis",
        );
        let issued = self.analysis_issued_item(record)?;
        let mut origin = self.project_study_analysis(record, &issued, index)?;
        let disclosure = if record.attempt.status == AttemptStatus::Active {
            CardDisclosure::SourceOnly
        } else {
            CardDisclosure::Full
        };
        origin["cardDetails"] = json!(self.project_card_details(record, &issued, disclosure)?);
        Ok(origin)
    }

    fn project_study_analysis(
        &self,
        record: &StoredAttempt,
        issued: &ContentItem,
        index: usize,
    ) -> Result<Value, AppError> {
        let attempt = &record.attempt;
        let dataset = &attempt.dataset;
        let attempt_id = &attempt.id;
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
            "attemptId": attempt_id, "revision": attempt.revision, "contentVersion": attempt.content_version,
        }))
    }

    /// Export only on explicit request. The ordinary board can open without
    /// fetching the current document or interpreting its PGN.
    pub fn study_analysis_line(
        &self,
        dataset: &DatasetId,
        attempt_id: &AttemptId,
        revision: Revision,
        version: ContentVersion,
    ) -> Result<StudyAnalysisLine, AppError> {
        let _diagnostic =
            crate::diagnostics::Span::new("app.study_analysis_context.study_analysis_line");
        let (record, issued) = self.authorized_analysis(dataset, attempt_id, revision, version)?;
        let definition = &issued.definition;
        match &definition.kind {
            ContentKind::Tactic { .. } => {
                let line = if record.attempt.state.played_mate.is_some() {
                    tempo_chess::revealed_attempt_line(definition, &record.attempt.state)
                } else {
                    revealed_line(definition, definition.decisions.len())
                }
                .map_err(|error| domain("invalid_content", error))?;
                Ok(StudyAnalysisLine::FullLine(line))
            }
            ContentKind::Repertoire {
                repertoire,
                position,
            } => {
                let document = self.store.repertoire_document(repertoire)?;
                if &document.dataset != dataset {
                    return Err(AppError::new("stale_dataset", "The Study dataset changed."));
                }
                let decision = definition.decisions.first().ok_or_else(|| {
                    AppError::new("invalid_content", "The exercise has no decision.")
                })?;
                if let Some(pgn) = document.pgn {
                    let matched =
                        first_matching_repertoire_line(&pgn, position, &decision.expected.uci)
                            .map_err(|_| {
                                AppError::new(
                                    "analysis_line_invalid",
                                    "Tempo could not read the saved variation.",
                                )
                            })?;
                    if let Some(line) = matched {
                        return Ok(StudyAnalysisLine::FullLine(line));
                    }
                } else if document.source_id.is_some() {
                    return Err(AppError::new(
                        "analysis_line_unavailable",
                        "The saved variation is unavailable.",
                    ));
                }
                let line = revealed_line(definition, 1)
                    .map_err(|error| domain("invalid_content", error))?;
                Ok(StudyAnalysisLine::IssuedMove(line))
            }
        }
    }

    fn authorized_analysis(
        &self,
        dataset: &DatasetId,
        attempt_id: &AttemptId,
        revision: Revision,
        version: ContentVersion,
    ) -> Result<(StoredAttempt, ContentItem), AppError> {
        let mut state = self.store.study_state(StudyScope {
            attempt: Some(attempt_id),
            ..StudyScope::default()
        })?;
        let record = state.attempts.remove(attempt_id).ok_or_else(|| {
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
        analysis_disclosure(&record)?;
        let issued = self.analysis_issued_item(&record)?;
        Ok((record, issued))
    }

    fn analysis_issued_item(&self, record: &StoredAttempt) -> Result<ContentItem, AppError> {
        let attempt = &record.attempt;
        self.store
            .issued_item(&attempt.dataset, &attempt.target, attempt.content_version)?
            .ok_or_else(|| AppError::new("stale_content", "The issued content is unavailable."))
    }
}

pub(super) fn analysis_disclosure(record: &StoredAttempt) -> Result<(), AppError> {
    let attempt = &record.attempt;
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
    Ok(())
}
