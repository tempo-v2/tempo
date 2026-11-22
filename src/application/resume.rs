//! Read current attempt progress without grading or writing new learning.
use super::{
    Tempo,
    current_attempt::{check_attempt_dataset, check_current_target},
};
use crate::AppError;
use tempo_store::{ActivityMode, AnswerResult, ResumeState, StoreError, StoredAttempt, StudyScope};
use tempo_types::AttemptId;

impl Tempo {
    /// Next resumes retained issued work before reading any selection inputs.
    pub(super) fn active_next_record(&mut self) -> Result<Option<StoredAttempt>, AppError> {
        let state = self.store.active_resume_state()?;
        let Some(id) = state.active_attempt.as_ref() else {
            return Ok(None);
        };
        let record = state.record.as_ref().ok_or_else(missing_attempt)?;
        check_attempt_dataset(record, &state.dataset)?;
        if record.attempt.id != *id || record.attempt.status != tempo_study::AttemptStatus::Active {
            return Err(AppError::new(
                "stale_activity",
                "activity is no longer current",
            ));
        }
        if record.repertoire_warmup || record.mode == ActivityMode::Practice {
            // The existing no-grade retirement transaction needs only its
            // selected legacy attempt. Retained work never enters this path.
            let snapshot = self.store.study_state(StudyScope::default())?;
            if snapshot.dataset != state.dataset || !self.retire_legacy_active(&snapshot, id)? {
                return Err(
                    StoreError::Conflict("Active activity changed. Retry Next.".into()).into(),
                );
            }
            return Ok(None);
        }
        let id = id.clone();
        checked_record(state, &id).map(Some)
    }

    pub(super) fn next_after_selection_race(&mut self) -> Result<StoredAttempt, AppError> {
        self.active_next_record()?.ok_or_else(|| {
            StoreError::Conflict("Active activity changed. Retry Next.".into()).into()
        })
    }

    /// Read an issued attempt and compatibility facts without a write.
    pub fn resume(&self, id: &AttemptId) -> Result<AnswerResult, AppError> {
        resume_result(&self.resume_record(id)?)
    }

    pub(super) fn resume_record(&self, id: &AttemptId) -> Result<StoredAttempt, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.resume.resume");
        checked_record(self.store.resume_state(id)?, id)
    }
}

fn missing_attempt() -> AppError {
    AppError::new("unknown_attempt", "attempt does not exist")
}

fn checked_record(state: ResumeState, id: &AttemptId) -> Result<StoredAttempt, AppError> {
    let record = state.record.ok_or_else(missing_attempt)?;
    check_attempt_dataset(&record, &state.dataset)?;
    check_current_target(
        &record,
        id,
        state.active_attempt.as_ref(),
        state.target.as_ref(),
    )?;
    Ok(record)
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
