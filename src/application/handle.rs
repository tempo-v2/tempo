//! Dispatch a transport intent and project the board from the issued definition.
use super::Tempo;
use crate::{
    AppError,
    app_error::domain,
    commands::StudyCommand,
    views::attempt_view::CorrectionView,
    views::{LessonView, StudyView},
};
use tempo_chess::{
    ChessDefinition, ContentKind, RepertoireLesson, TacticExplanation, explain_issued_tactic,
    move_san, project_position,
};
use tempo_store::{ActivityMode, AnswerResult, StoredAttempt, StudyLessonRead, StudyScope};
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
            StudyCommand::PracticeTactics => {
                Err(super::optional_tactics::retired_optional_tactics())
            }
            StudyCommand::LearnMoreTactics {
                dataset_id,
                submission_id,
            } => self.replay_retired_allowance(dataset_id, submission_id),
            StudyCommand::ResumeSkipped => {
                if let Some(daily) = &mut self.daily {
                    daily.queue.resume_skipped();
                }
                self.next_view()
            }
            StudyCommand::Resume { attempt_id } => self.project_resume(&attempt_id),
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
        if let Some(record) = self.active_next_record()? {
            return self.project_resume_record(&record);
        }
        let snapshot = self.store.study_state(StudyScope {
            catalog: true,
            ..StudyScope::default()
        })?;
        if snapshot.active_attempt.is_some() {
            let record = self.next_after_selection_race()?;
            return self.project_resume_record(&record);
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
        self.project_stored_result(result, source, &record)
    }

    fn project_resume(&self, id: &tempo_types::AttemptId) -> Result<StudyView, AppError> {
        self.project_resume_record(&self.resume_record(id)?)
    }

    fn project_resume_record(&self, record: &StoredAttempt) -> Result<StudyView, AppError> {
        let result = super::resume::resume_result(record)?;
        let attempt = &result.attempt;
        let lesson_read = if guided(record, &attempt.status) && record.lesson.is_none() {
            StudyLessonRead::Guided
        } else {
            StudyLessonRead::Omit
        };
        let issued = self
            .store
            .issued_study_definition(
                &attempt.dataset,
                &attempt.target,
                attempt.content_version,
                lesson_read,
            )?
            .ok_or_else(|| AppError::new("invalid_content", "issued definition is missing"))?;
        self.project_definition(
            result,
            record,
            &issued.exercise.definition,
            issued.legacy_lesson.as_ref(),
            |_| None,
        )
    }

    fn project_stored_result(
        &self,
        result: AnswerResult,
        source: FeedbackSource,
        record: &StoredAttempt,
    ) -> Result<StudyView, AppError> {
        let attempt = &result.attempt;
        // Immutable definitions keep replay deterministic after content replacement.
        let issued = self
            .store
            .issued_item(&attempt.dataset, &attempt.target, attempt.content_version)?
            .ok_or_else(|| AppError::new("invalid_content", "issued definition is missing"))?;
        let explains = matches!(
            source,
            FeedbackSource::SubmittedMove | FeedbackSource::RevealedMove
        ) && attempt.status == AttemptStatus::Active
            && matches!(record.mode, ActivityMode::Test | ActivityMode::Practice)
            && matches!(&issued.definition.kind, ContentKind::Tactic { .. });
        self.project_definition(
            result,
            record,
            &issued.definition,
            issued.lesson.as_ref(),
            |index| {
                if explains {
                    explain_issued_tactic(&issued, index).ok().flatten()
                } else {
                    None
                }
            },
        )
    }

    fn project_definition(
        &self,
        result: AnswerResult,
        record: &StoredAttempt,
        definition: &ChessDefinition,
        legacy_lesson: Option<&RepertoireLesson>,
        explain: impl FnOnce(usize) -> Option<TacticExplanation>,
    ) -> Result<StudyView, AppError> {
        let attempt = &result.attempt;
        let board = project_position(
            definition,
            &attempt.state,
            attempt.status == AttemptStatus::Active,
        )
        .map_err(|error| domain("invalid_content", error))?;
        let correction = if result.feedback.as_ref().is_some_and(|f| !f.correct) {
            // A miss leaves the cursor on the same decision, including on replay
            // of a receipt whose pinned definition has since been replaced.
            let decision = definition
                .decisions
                .get(attempt.state.decision_index)
                .ok_or_else(|| {
                    AppError::new("invalid_content", "correction decision is missing")
                })?;
            let uci = &decision.expected.uci;
            let san =
                move_san(&decision.fen, uci).map_err(|error| domain("invalid_content", error))?;
            let tactic_explanation = explain(attempt.state.decision_index);
            Some(CorrectionView {
                uci: uci.clone(),
                san,
                tactic_explanation,
            })
        } else {
            None
        };
        let lesson = guided(record, &attempt.status)
            .then(|| {
                let decision = &definition.decisions[0];
                let expected = &decision.expected.uci;
                let san = move_san(&decision.fen, expected)
                    .map_err(|error| domain("invalid_content", error))?;
                let lesson = record.lesson.as_ref().or(legacy_lesson);
                Ok::<_, AppError>(LessonView {
                    move_uci: expected.clone(),
                    move_san: san,
                    comments: lesson.map_or_else(Vec::new, |lesson| lesson.comments.clone()),
                    previous_move: lesson.and_then(|lesson| lesson.previous_move.clone()),
                })
            })
            .transpose()?;
        let history = (attempt.state.decision_index > 0)
            .then(|| tempo_chess::revealed_attempt_line(definition, &attempt.state))
            .transpose()
            .map_err(|error| domain("invalid_content", error))?;
        Ok(StudyView::attempt(
            result, board, correction, record, lesson, history,
        ))
    }
}

fn guided(record: &StoredAttempt, status: &AttemptStatus) -> bool {
    status == &AttemptStatus::Active
        && matches!(record.mode, ActivityMode::Teach | ActivityMode::Repair)
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
