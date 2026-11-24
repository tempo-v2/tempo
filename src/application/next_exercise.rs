//! Issue or resume the daily activity without projecting the wire view.
use super::Tempo;
use crate::AppError;
use tempo_store::{AnswerResult, StudyScope};

impl Tempo {
    /// Resume the active attempt or issue the next daily activity.
    pub fn next_exercise(&mut self) -> Result<Option<AnswerResult>, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.next_exercise.next_exercise");
        if let Some(record) = self.active_next_record()? {
            return super::resume::resume_result(&record).map(Some);
        }
        let snapshot = self.store.study_state(StudyScope {
            catalog: true,
            ..StudyScope::default()
        })?;
        if snapshot.active_attempt.is_some() {
            let record = self.next_after_selection_race()?;
            return super::resume::resume_result(&record).map(Some);
        }
        let at = self.components.clock.now();
        let extra = self.store.tactics_extra(&snapshot.dataset, at.day)?;
        let selection = self.daily_selection(&snapshot, at, extra)?;
        self.issue_next_daily(&snapshot, at, &selection)
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn next_resumes_an_active_exercise_instead_of_issuing_a_competing_one() {
        let (mut app, _directory) = crate::components::test_study();
        let first = app.next_exercise().unwrap().unwrap();
        let revision = app.snapshot().unwrap().revision;
        let next = app.next_exercise().unwrap().unwrap();
        assert_eq!(next.attempt, first.attempt);
        assert_eq!(app.snapshot().unwrap().revision, revision);
    }
}
