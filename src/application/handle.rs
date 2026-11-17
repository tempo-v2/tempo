//! Dispatch a transport intent and project the board from the issued definition.
use super::Tempo;
use crate::{
    AppError,
    app_error::domain,
    commands::StudyCommand,
    views::{EmptyView, StudyView},
    wire::STUDY_WIRE_VERSION,
};
use tempo_chess::project_position;
use tempo_store::AnswerResult;
use tempo_study::AttemptStatus;

impl Tempo {
    /// Handle one decoded transport intent, returning an attempt, empty, or error view.
    pub fn handle(&mut self, command: StudyCommand) -> StudyView {
        let result: Result<StudyView, AppError> = match command {
            StudyCommand::Next => self.next_exercise().and_then(|value| match value {
                Some(value) => self.project(value),
                None => Ok(StudyView::Empty(EmptyView {
                    schema_version: STUDY_WIRE_VERSION,
                    status: "empty".into(),
                    dataset_id: self.snapshot()?.dataset,
                })),
            }),
            StudyCommand::Resume { attempt_id } => self
                .resume(&attempt_id)
                .and_then(|value| self.project(value)),
            StudyCommand::Answer(command) => {
                self.submit(command).and_then(|value| self.project(value))
            }
            StudyCommand::Abandon(command) => {
                self.abandon(command).and_then(|value| self.project(value))
            }
        };
        result.unwrap_or_else(Into::into)
    }

    fn project(&self, result: AnswerResult) -> Result<StudyView, AppError> {
        let snapshot = self.snapshot()?;
        let attempt = &result.attempt;
        // Receipt replay can return a definition from before a semantic import
        // or dataset restore. Resume already checks the current catalog.
        let issued = snapshot
            .content_history
            .iter()
            .find(|entry| {
                entry.dataset == attempt.dataset
                    && entry.item.target == attempt.target
                    && entry.item.version == attempt.content_version
            })
            .ok_or_else(|| AppError::new("invalid_content", "issued definition is missing"))?;
        let board = project_position(
            &issued.item.definition,
            &attempt.state,
            attempt.status == AttemptStatus::Active,
        )
        .map_err(|error| domain("invalid_content", error))?;
        let correction_uci = if result.feedback.as_ref().is_some_and(|f| !f.correct) {
            // A miss leaves the cursor on the same decision, including on replay
            // of a receipt whose pinned definition has since been replaced.
            let decision = issued
                .item
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
        Ok(StudyView::attempt(result, board, correction_uci))
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
