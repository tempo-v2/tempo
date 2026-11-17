//! Ask the queue-backed question source for an exercise, or resume the active one.
use super::{Tempo, queue_source::QueueSource};
use crate::{AppError, app_error::domain};
use tempo_queue::QueueOrderContext;
use tempo_store::{AnswerResult, StoredAttempt, Write};
use tempo_study::Availability;

impl Tempo {
    /// Resume the active attempt, otherwise ask the queue for the next exercise.
    ///
    /// `None` means no exercise is eligible now, not that the study service is
    /// permanently finished. Issuance and the active pointer are committed together.
    pub fn next_exercise(&mut self) -> Result<Option<AnswerResult>, AppError> {
        let snapshot = self.store.snapshot()?;
        if let Some(id) = &snapshot.active_attempt {
            return self.resume_from(&snapshot, id).map(Some);
        }
        let context = QueueOrderContext {
            seed: snapshot.revision.0,
            history: self.order_history.clone(),
        };
        let mut source = QueueSource {
            snapshot: &snapshot,
            queue: self.components.queue.as_ref(),
            grader: self.components.grader.as_ref(),
            today: self.components.clock.now().day,
            context,
            remaining: self.remaining_block.clone(),
        };
        let id = self.components.ids.next_id();
        let result = tempo_study::next_available_exercise_if_any(
            &mut source,
            self.components.grader.as_ref(),
            snapshot.dataset.clone(),
            id,
        )
        .map_err(|e| domain("source_or_grader_error", e))?;
        let remaining = source.remaining;
        let Availability::Ready(started) = result else {
            return Ok(None);
        };
        let state = snapshot
            .learning
            .get(&started.attempt.target)
            .ok_or_else(|| {
                AppError::new("invalid_learning", "selected target has no learning state")
            })?;
        let row = StoredAttempt {
            attempt: started.attempt,
            issued_at: self.components.clock.now(),
            issued_miss_revision: state.miss_revision,
        };
        self.commit(
            &snapshot,
            vec![
                Write::Issue(row.clone()),
                Write::Active(Some(row.attempt.id.clone())),
            ],
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
