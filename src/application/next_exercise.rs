//! Ask the queue-backed question source for an exercise, or resume the active one.
use super::{Tempo, queue_source::QueueSource};
use crate::{AppError, app_error::domain};
use tempo_queue::QueueOrderContext;
use tempo_store::{AnswerResult, StoredAttempt, StudyScope, Write};
use tempo_study::Availability;

impl Tempo {
    /// Resume the active attempt, otherwise ask the queue for the next exercise.
    ///
    /// `None` means no exercise is eligible now, not that the study service is
    /// permanently finished. Issuance and the active pointer are committed together.
    pub fn next_exercise(&mut self) -> Result<Option<AnswerResult>, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.next_exercise.next_exercise");
        let scope = StudyScope {
            catalog: true,
            ..StudyScope::default()
        };
        let snapshot = self.store.study_state(scope)?;
        if let Some(id) = &snapshot.active_attempt {
            return self.resume_from(&snapshot, id).map(Some);
        }
        if self.daily.is_some() {
            let at = self.components.clock.now();
            let extra = self.store.tactics_extra(&snapshot.dataset, at.day)?;
            let selection = self.daily_selection(&snapshot, at, extra)?;
            return self.issue_next_daily(&snapshot, at, &selection, false);
        }
        let at = self.components.clock.now();
        let context = QueueOrderContext {
            seed: snapshot.revision.0,
            history: self.order_history.clone(),
        };
        let (catalog, learning) = snapshot.complete_catalog()?;
        let mut source = QueueSource {
            catalog,
            excluded: &snapshot.excluded_tactics,
            learning,
            queue: self.fixture.as_ref().expect("fixture app").queue.as_ref(),
            today: at.day,
            context,
            remaining: self.remaining_block.clone(),
        };
        let id = self.components.ids.next_id();
        let result = crate::diagnostics::measure("study.next_available_exercise_if_any", || {
            tempo_study::next_available_exercise_if_any(
                &mut source,
                self.components.grader.as_ref(),
                snapshot.dataset.clone(),
                id,
            )
        })
        .map_err(|e| domain("source_or_grader_error", e))?;
        let remaining = source.remaining;
        let Availability::Ready(started) = result else {
            return Ok(None);
        };
        let state = learning.get(&started.attempt.target).ok_or_else(|| {
            AppError::new("invalid_learning", "selected target has no learning state")
        })?;
        let opponent_lead_in = catalog
            .items
            .get(&started.attempt.target)
            .and_then(tempo_chess::ContentItem::verified_opponent_lead_in);
        let row = StoredAttempt {
            attempt: started.attempt,
            issued_at: at,
            issued_revision: Some(snapshot.revision),
            issued_miss_revision: state.miss_revision,
            mode: tempo_store::ActivityMode::Test,
            route: None,
            first_introduction: false,
            exercise_name: None,
            relevance_reason: None,
            lesson: None,
            opponent_lead_in,
            repertoire_warmup: false,
            acceptance: None,
            revealed_decision: None,
        };
        self.commit_study(
            &snapshot,
            vec![
                Write::Issue(row.clone()),
                Write::Active(Some(row.attempt.id.clone())),
            ],
            StudyScope {
                target: Some(&row.attempt.target),
                ..Default::default()
            },
        )?;
        self.remaining_block = remaining;
        Ok(Some(AnswerResult {
            attempt: row.attempt,
            feedback: None,
            assessment: None,
            learning_effect: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::Tempo;

    #[test]
    fn next_resumes_an_active_exercise_instead_of_issuing_a_competing_one() {
        let mut app = Tempo::demo().unwrap();
        let first = app.next_exercise().unwrap().unwrap();
        let revision = app.snapshot().unwrap().revision;
        let next = app.next_exercise().unwrap().unwrap();
        assert_eq!(next.attempt, first.attempt);
        assert_eq!(app.snapshot().unwrap().revision, revision);
    }
}
