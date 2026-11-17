//! The three reply shapes a presentation client must handle.
use crate::AppError;
use crate::views::{AttemptView, EmptyView, ErrorView};
use serde::{Deserialize, Serialize};
use tempo_store::AnswerResult;
use tempo_study::AttemptStatus;

/// A versioned reply carrying a displayable attempt, an empty queue, or an error.
///
/// [`crate::Tempo::handle`] produces this; transports serialize it and clients
/// branch on `status`. The conversion from an answer result carries the attempt's
/// lifetime terminal grade, even when a corrective move emits no *new* assessment.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StudyView {
    Attempt(AttemptView),
    Empty(EmptyView),
    Error(ErrorView),
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

#[cfg(test)]
mod tests {
    use super::StudyView;
    use crate::{SubmitCommand, Tempo};
    use tempo_types::Grade;

    #[test]
    fn a_resumed_correction_still_displays_the_attempts_original_again_grade() {
        let mut app = Tempo::demo().unwrap();
        let issued = app.next_exercise().unwrap().unwrap();
        app.submit(SubmitCommand {
            dataset_id: issued.attempt.dataset,
            attempt_id: issued.attempt.id.clone(),
            submission_id: "mistake".into(),
            expected_revision: issued.attempt.revision,
            answer: "e2e3".into(),
        })
        .unwrap();
        let resumed = app.resume(&issued.attempt.id).unwrap();
        assert_eq!(resumed.assessment, None); // no new assessment from a read
        let StudyView::Attempt(view) = StudyView::from(resumed) else {
            panic!("attempt view")
        };
        assert_eq!(view.status, "ready");
        assert_eq!(view.assessment, Some(Grade::Again));
    }
}
