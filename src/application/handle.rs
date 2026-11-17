//! Dispatch a transport intent and map its result to the version 1 view.
use super::Tempo;
use crate::{
    AppError,
    commands::StudyCommand,
    views::{EmptyView, StudyView},
};

impl Tempo {
    /// Handle one decoded transport intent, returning an attempt, empty, or error view.
    pub fn handle(&mut self, command: StudyCommand) -> StudyView {
        let result: Result<StudyView, AppError> = match command {
            StudyCommand::Next => self.next_exercise().and_then(|value| match value {
                Some(value) => Ok(value.into()),
                None => Ok(StudyView::Empty(EmptyView {
                    schema_version: 1,
                    status: "empty".into(),
                    dataset_id: self.snapshot()?.dataset,
                })),
            }),
            StudyCommand::Resume { attempt_id } => self.resume(&attempt_id).map(Into::into),
            StudyCommand::Answer(command) => self.submit(command).map(Into::into),
            StudyCommand::Abandon(command) => self.abandon(command).map(Into::into),
        };
        result.unwrap_or_else(Into::into)
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
