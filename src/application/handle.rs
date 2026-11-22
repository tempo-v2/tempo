//! Dispatch a transport intent and project the board from the issued definition.
use super::Tempo;
use crate::{
    AppError,
    app_error::domain,
    commands::StudyCommand,
    views::attempt_view::CorrectionView,
    views::{LessonView, StudyView},
};
use tempo_chess::{ContentKind, explain_issued_tactic, move_san, project_position};
use tempo_store::{ActivityMode, AnswerResult, StudyScope};
use tempo_study::AttemptStatus;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FeedbackSource {
    SubmittedMove,
    RevealedMove,
    Other,
}

impl Tempo {
    /// Handle one decoded transport intent, returning an attempt, empty, or error view.
    pub fn handle(&mut self, command: StudyCommand) -> StudyView {
        let _diagnostic = crate::diagnostics::Span::new("app.handle.handle");
        let result: Result<StudyView, AppError> = match command {
            StudyCommand::Next => self.next_view(),
            StudyCommand::StudyStatus => self.study_status(),
            StudyCommand::PracticeTactics => self.practice_tactics(),
            StudyCommand::LearnMoreTactics {
                dataset_id,
                submission_id,
            } => self.learn_more_tactics(dataset_id, submission_id),
            StudyCommand::ResumeSkipped => {
                if let Some(daily) = &mut self.daily {
                    daily.queue.resume_skipped();
                }
                self.next_view()
            }
            StudyCommand::Resume { attempt_id } => self
                .resume(&attempt_id)
                .and_then(|value| self.project(value)),
            StudyCommand::Answer(command) => self
                .submit(command)
                .and_then(|value| self.project_result(value, FeedbackSource::SubmittedMove)),
            StudyCommand::Acknowledge(command) => self
                .acknowledge(command)
                .and_then(|value| self.project(value)),
            StudyCommand::GiveUp(command) => self
                .give_up(command)
                .and_then(|value| self.project_result(value, FeedbackSource::RevealedMove)),
            StudyCommand::Concede(command) => {
                self.concede(command).and_then(|value| self.project(value))
            }
            StudyCommand::Skip(command) => self.skip(command).and_then(|value| self.project(value)),
            StudyCommand::Abandon(command) => {
                self.abandon(command).and_then(|value| self.project(value))
            }
        };
        result
            .unwrap_or_else(Into::into)
            .without_repertoire_context()
    }

    fn study_status(&self) -> Result<StudyView, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.handle.study_status");
        let snapshot = self.store.study_state(StudyScope {
            catalog: true,
            ..StudyScope::default()
        })?;
        let at = self.components.clock.now();
        let extra = self.store.tactics_extra(&snapshot.dataset, at.day)?;
        let selection = self.daily_selection(&snapshot, at, extra)?;
        Ok(self.daily_summary(snapshot.dataset, selection))
    }

    fn next_view(&mut self) -> Result<StudyView, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.handle.next_view");
        let active = self.store.study_state(StudyScope::default())?;
        if let Some(id) = &active.active_attempt
            && !self.retire_active_warmup(&active, id)?
        {
            return self.project(self.resume_from(&active, id)?);
        }
        let snapshot = self.store.study_state(StudyScope {
            catalog: true,
            ..StudyScope::default()
        })?;
        if let Some(id) = &snapshot.active_attempt {
            return self.project(self.resume_from(&snapshot, id)?);
        }
        let at = self.components.clock.now();
        let extra = self.store.tactics_extra(&snapshot.dataset, at.day)?;
        let selection = self.daily_selection(&snapshot, at, extra)?;
        if let Some(issued) = self.issue_next_daily(&snapshot, at, &selection)? {
            return self.project(issued);
        }
        Ok(self.daily_summary(snapshot.dataset, selection))
    }

    pub(super) fn project(&self, result: AnswerResult) -> Result<StudyView, AppError> {
        self.project_result(result, FeedbackSource::Other)
    }

    fn project_result(
        &self,
        result: AnswerResult,
        source: FeedbackSource,
    ) -> Result<StudyView, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.handle.project");
        let attempt = &result.attempt;
        let record = self
            .store
            .stored_attempt(&attempt.id)?
            .ok_or_else(|| AppError::new("unknown_attempt", "Issued activity is missing."))?;
        // Immutable definitions keep replay deterministic after content replacement.
        let issued = self
            .store
            .issued_item(&attempt.dataset, &attempt.target, attempt.content_version)?
            .ok_or_else(|| AppError::new("invalid_content", "issued definition is missing"))?;
        let board = project_position(
            &issued.definition,
            &attempt.state,
            attempt.status == AttemptStatus::Active,
        )
        .map_err(|error| domain("invalid_content", error))?;
        let correction = if result.feedback.as_ref().is_some_and(|f| !f.correct) {
            // A miss leaves the cursor on the same decision, including on replay
            // of a receipt whose pinned definition has since been replaced.
            let decision = issued
                .definition
                .decisions
                .get(attempt.state.decision_index)
                .ok_or_else(|| {
                    AppError::new("invalid_content", "correction decision is missing")
                })?;
            let uci = &decision.expected.uci;
            let san =
                move_san(&decision.fen, uci).map_err(|error| domain("invalid_content", error))?;
            let tactic_explanation = if matches!(
                source,
                FeedbackSource::SubmittedMove | FeedbackSource::RevealedMove
            ) && attempt.status == AttemptStatus::Active
                && matches!(record.mode, ActivityMode::Test | ActivityMode::Practice)
                && matches!(&issued.definition.kind, ContentKind::Tactic { .. })
            {
                explain_issued_tactic(&issued, attempt.state.decision_index)
                    .ok()
                    .flatten()
            } else {
                None
            };
            Some(CorrectionView {
                uci: uci.clone(),
                san,
                tactic_explanation,
            })
        } else {
            None
        };
        let lesson = (matches!(record.mode, ActivityMode::Teach | ActivityMode::Repair)
            && attempt.status == AttemptStatus::Active)
            .then(|| {
                let decision = &issued.definition.decisions[0];
                let expected = &decision.expected.uci;
                let san = move_san(&decision.fen, expected)
                    .map_err(|error| domain("invalid_content", error))?;
                let lesson = record.lesson.as_ref().or(issued.lesson.as_ref());
                Ok::<_, AppError>(LessonView {
                    move_uci: expected.clone(),
                    move_san: san,
                    comments: lesson.map_or_else(Vec::new, |lesson| lesson.comments.clone()),
                    previous_move: lesson.and_then(|lesson| lesson.previous_move.clone()),
                })
            })
            .transpose()?;
        let history = (attempt.state.decision_index > 0)
            .then(|| tempo_chess::revealed_attempt_line(&issued.definition, &attempt.state))
            .transpose()
            .map_err(|error| domain("invalid_content", error))?;
        Ok(StudyView::attempt(
            result, board, correction, &record, lesson, history,
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::wire::{StudyCommand, StudyView};

    #[test]
    fn a_command_failure_becomes_a_serializable_error_view() {
        let (mut app, _directory) = crate::components::test_study();
        let view = app.handle(StudyCommand::Resume {
            attempt_id: "unknown".into(),
        });
        let StudyView::Error(error) = view else {
            panic!("expected an error reply")
        };
        assert_eq!(error.code, "unknown_attempt");
        assert!(!error.retryable);
        // A failed request has not made later study requests fail.
        assert!(matches!(
            app.handle(StudyCommand::Next),
            StudyView::Attempt(_)
        ));
    }
}
