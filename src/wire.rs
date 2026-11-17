//! Versioned UI contract. This is the only boundary that serializes view/intent messages.
use crate::{AbandonCommand, AppError, SubmitCommand, Tempo};
use serde::{Deserialize, Serialize};
use tempo_chess::{ChessFeedback, ChessPrompt};
use tempo_store::AnswerResult;
use tempo_study::AttemptStatus;
use tempo_types::{AttemptId, ContentVersion, DatasetId, Grade, Revision, TargetId};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum StudyCommand {
    Next,
    Resume { attempt_id: AttemptId },
    Answer(SubmitCommand),
    Abandon(AbandonCommand),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StudyView {
    Attempt(AttemptView),
    Empty(EmptyView),
    Error(ErrorView),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttemptView {
    pub schema_version: u32,
    pub status: String,
    pub dataset_id: DatasetId,
    pub attempt_id: AttemptId,
    pub target_id: TargetId,
    pub content_version: ContentVersion,
    pub revision: Revision,
    pub prompt: Option<ChessPrompt>,
    pub feedback: Option<ChessFeedback>,
    pub assessment: Option<Grade>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmptyView {
    pub schema_version: u32,
    pub status: String,
    pub dataset_id: DatasetId,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorView {
    pub schema_version: u32,
    pub status: String,
    pub code: String,
    pub message: String,
    pub retryable: bool,
}
impl From<AppError> for StudyView {
    fn from(error: AppError) -> Self {
        Self::Error(ErrorView {
            schema_version: 1,
            status: "error".into(),
            code: error.code,
            message: error.message,
            retryable: error.retryable,
        })
    }
}
impl From<AnswerResult> for StudyView {
    fn from(value: AnswerResult) -> Self {
        let attempt = value.attempt;
        let status = match attempt.status {
            AttemptStatus::Active => "ready",
            AttemptStatus::Complete => "complete",
            AttemptStatus::Abandoned => "abandoned",
        };
        Self::Attempt(AttemptView {
            schema_version: 1,
            status: status.into(),
            dataset_id: attempt.dataset,
            attempt_id: attempt.id,
            target_id: attempt.target,
            content_version: attempt.content_version,
            revision: attempt.revision,
            prompt: attempt.prompt,
            feedback: value.feedback,
            assessment: attempt.terminal,
        })
    }
}
impl Tempo {
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
