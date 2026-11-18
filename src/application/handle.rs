//! Dispatch a transport intent and project the board from the issued definition.
use super::Tempo;
use crate::{
    AppError,
    app_error::domain,
    commands::StudyCommand,
    views::{EmptyView, LessonView, StudyView},
    wire::STUDY_WIRE_VERSION,
};
use tempo_chess::project_position;
use tempo_store::{ActivityMode, AnswerResult, StudyScope};
use tempo_study::AttemptStatus;

impl Tempo {
    /// Handle one decoded transport intent, returning an attempt, empty, or error view.
    pub fn handle(&mut self, command: StudyCommand) -> StudyView {
        let result: Result<StudyView, AppError> = match command {
            StudyCommand::Next => self.next_view(),
            StudyCommand::ResumeSkipped => {
                if let Some(daily) = &mut self.daily {
                    daily.queue.resume_skipped();
                }
                self.next_view()
            }
            StudyCommand::Resume { attempt_id } => self
                .resume(&attempt_id)
                .and_then(|value| self.project(value)),
            StudyCommand::Answer(command) => {
                self.submit(command).and_then(|value| self.project(value))
            }
            StudyCommand::Acknowledge(command) => self
                .acknowledge(command)
                .and_then(|value| self.project(value)),
            StudyCommand::GiveUp(command) => {
                self.give_up(command).and_then(|value| self.project(value))
            }
            StudyCommand::Skip(command) => self.skip(command).and_then(|value| self.project(value)),
            StudyCommand::Abandon(command) => {
                self.abandon(command).and_then(|value| self.project(value))
            }
        };
        result.unwrap_or_else(Into::into)
    }

    fn next_view(&mut self) -> Result<StudyView, AppError> {
        let active = self.store.study_state(StudyScope::default())?;
        if let Some(id) = &active.active_attempt {
            return self.project(self.resume_from(&active, id)?);
        }
        let empty = |dataset_id, outstanding, repertoire_remaining, tactics_remaining| {
            StudyView::Empty(EmptyView {
                schema_version: STUDY_WIRE_VERSION,
                status: "empty".into(),
                dataset_id,
                done: outstanding == 0,
                outstanding,
                repertoire_remaining,
                tactics_remaining,
            })
        };
        if self.daily.is_some() {
            let snapshot = self.store.study_state(StudyScope {
                catalog: true,
                ..StudyScope::default()
            })?;
            if let Some(id) = &snapshot.active_attempt {
                return self.project(self.resume_from(&snapshot, id)?);
            }
            let at = self.components.clock.now();
            let selection = self.daily_selection(&snapshot, at);
            if let Some(choice) = selection.next {
                let result = self.issue_daily(&snapshot, at, choice)?;
                return self.project(result);
            }
            return Ok(empty(
                snapshot.dataset,
                selection.outstanding,
                selection.repertoire_remaining,
                selection.tactics_remaining,
            ));
        }
        match self.next_exercise()? {
            Some(value) => self.project(value),
            None => Ok(empty(active.dataset, 0, 0, 0)),
        }
    }

    fn project(&self, result: AnswerResult) -> Result<StudyView, AppError> {
        let attempt = &result.attempt;
        let state = self.store.study_state(StudyScope {
            attempt: Some(&attempt.id),
            ..StudyScope::default()
        })?;
        let record = state
            .attempts
            .get(&attempt.id)
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
        let correction_uci = if result.feedback.as_ref().is_some_and(|f| !f.correct) {
            // A miss leaves the cursor on the same decision, including on replay
            // of a receipt whose pinned definition has since been replaced.
            let decision = issued
                .definition
                .decisions
                .get(attempt.state.decision_index)
                .ok_or_else(|| {
                    AppError::new("invalid_content", "correction decision is missing")
                })?;
            Some(decision.expected.uci.clone())
        } else {
            None
        };
        let lesson = (record.mode == ActivityMode::Teach
            && attempt.status == AttemptStatus::Active)
            .then(|| {
                let expected = &issued.definition.decisions[0].expected.uci;
                LessonView {
                    move_uci: expected.clone(),
                    move_san: issued
                        .lesson
                        .as_ref()
                        .map_or_else(|| expected.clone(), |lesson| lesson.san.clone()),
                    comments: issued
                        .lesson
                        .as_ref()
                        .map_or_else(Vec::new, |lesson| lesson.comments.clone()),
                    previous_move: issued.lesson.and_then(|lesson| lesson.previous_move),
                }
            });
        Ok(StudyView::attempt(
            result,
            board,
            correction_uci,
            record,
            lesson,
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        Tempo,
        wire::{StudyCommand, StudyView},
    };

    #[test]
    fn a_command_failure_becomes_a_serializable_error_view() {
        let mut app = Tempo::demo().unwrap();
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
