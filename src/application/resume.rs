//! Read current attempt progress without grading or writing new learning.
use super::Tempo;
use crate::AppError;
use tempo_store::{AnswerResult, StudyScope, StudyState};
use tempo_types::AttemptId;

impl Tempo {
    // Resume rebuilds a reply from persisted progress. It does not replay the
    // last feedback message or emit the attempt's assessment a second time.
    pub(super) fn resume_from(
        &self,
        snapshot: &StudyState,
        id: &AttemptId,
    ) -> Result<AnswerResult, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.resume.resume_from");
        let record = self.current(snapshot, &snapshot.dataset, id)?;
        if record.repertoire_warmup {
            return Err(super::abandon::retired_warmup());
        }
        Ok(AnswerResult {
            attempt: record.attempt.clone(),
            feedback: None,
            assessment: None,
            learning_effect: None,
        })
    }
    /// Read an existing attempt against the current catalog, without a write.
    pub fn resume(&self, id: &AttemptId) -> Result<AnswerResult, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.resume.resume");
        self.resume_from(
            &self.store.study_state(StudyScope {
                attempt: Some(id),
                ..StudyScope::default()
            })?,
            id,
        )
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn resume_reads_progress_without_changing_the_store() {
        let (mut app, _directory) = crate::components::test_study();
        let issued = app.next_exercise().unwrap().unwrap();
        let before = app.snapshot().unwrap();
        assert_eq!(
            app.resume(&issued.attempt.id).unwrap().attempt,
            issued.attempt
        );
        assert_eq!(app.snapshot().unwrap(), before);
        assert_eq!(
            app.resume(&"missing".into()).unwrap_err().code,
            "unknown_attempt"
        );
    }
}
