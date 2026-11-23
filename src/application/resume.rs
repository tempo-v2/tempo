//! Read current attempt progress without grading or writing new learning.
use super::{
    Tempo,
    current_attempt::{check_attempt_dataset, check_current_target},
};
use crate::AppError;
use tempo_store::{AnswerResult, StoredAttempt, StudyState};
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
        resume_result(record)
    }

    /// Read an issued attempt and compatibility facts without a write.
    pub fn resume(&self, id: &AttemptId) -> Result<AnswerResult, AppError> {
        resume_result(&self.resume_record(id)?)
    }

    pub(super) fn resume_record(&self, id: &AttemptId) -> Result<StoredAttempt, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.resume.resume");
        let state = self.store.resume_state(id)?;
        let record = state
            .record
            .ok_or_else(|| AppError::new("unknown_attempt", "attempt does not exist"))?;
        check_attempt_dataset(&record, &state.dataset)?;
        check_current_target(
            &record,
            id,
            state.active_attempt.as_ref(),
            state.target.as_ref(),
        )?;
        Ok(record)
    }
}

pub(super) fn resume_result(record: &StoredAttempt) -> Result<AnswerResult, AppError> {
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
