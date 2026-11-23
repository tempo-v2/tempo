//! Issue or resume the daily activity without projecting the wire view.
use super::Tempo;
use crate::AppError;
use tempo_store::{AnswerResult, StudyScope};

impl Tempo {
    /// Resume the active attempt or issue the next daily activity.
    pub fn next_exercise(&mut self) -> Result<Option<AnswerResult>, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.next_exercise.next_exercise");
        let scope = StudyScope {
            catalog: true,
            ..StudyScope::default()
        };
        let mut snapshot = self.store.study_state(scope)?;
        if let Some(id) = &snapshot.active_attempt
            && self.retire_active_warmup(&snapshot, id)?
        {
            snapshot = self.store.study_state(scope)?;
        }
        if let Some(id) = &snapshot.active_attempt {
            return self.resume_from(&snapshot, id).map(Some);
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
