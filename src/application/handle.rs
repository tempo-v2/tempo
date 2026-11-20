//! Dispatch a transport intent and project the board from the issued definition.
use super::Tempo;
use crate::{
    AppError,
    app_error::domain,
    commands::StudyCommand,
    views::{EmptyView, LessonView, StudyView},
    wire::STUDY_WIRE_VERSION,
};
use tempo_chess::{move_san, project_position};
use tempo_store::{ActivityMode, AnswerResult, StudyScope};
use tempo_study::AttemptStatus;

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
            StudyCommand::Answer(command) => {
                self.submit(command).and_then(|value| self.project(value))
            }
            StudyCommand::Acknowledge(command) => self
                .acknowledge(command)
                .and_then(|value| self.project(value)),
            StudyCommand::GiveUp(command) => {
                self.give_up(command).and_then(|value| self.project(value))
            }
            StudyCommand::Concede(command) => {
                self.concede(command).and_then(|value| self.project(value))
            }
            StudyCommand::Skip(command) => self.skip(command).and_then(|value| self.project(value)),
            StudyCommand::Abandon(command) => {
                self.abandon(command).and_then(|value| self.project(value))
            }
        };
        result.unwrap_or_else(Into::into)
    }

    fn study_status(&self) -> Result<StudyView, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.handle.study_status");
        if self.daily.is_none() {
            return Err(AppError::new("unsupported", "Use the local study release."));
        }
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
        if let Some(id) = &active.active_attempt {
            return self.project(self.resume_from(&active, id)?);
        }
        if self.daily.is_some() {
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
            if let Some(choice) = selection.next {
                let result = self.issue_daily(&snapshot, at, choice)?;
                return self.project(result);
            }
            return Ok(self.daily_summary(snapshot.dataset, selection));
        }
        match self.next_exercise()? {
            Some(value) => self.project(value),
            None => Ok(StudyView::Empty(EmptyView {
                schema_version: STUDY_WIRE_VERSION,
                status: "empty".into(),
                dataset_id: active.dataset,
                done: true,
                corrective_recalls_remaining: None,
                outstanding: 0,
                repertoire_remaining: 0,
                tactics_remaining: 0,
                can_practice_tactics: false,
                can_learn_more_tactics: false,
                learn_more_count: 0,
            })),
        }
    }

    pub(super) fn project(&self, result: AnswerResult) -> Result<StudyView, AppError> {
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
            Some((uci.clone(), san))
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
