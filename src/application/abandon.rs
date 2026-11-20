//! Stop an attempt and save the reply for safe retries.
use super::{
    Tempo,
    receipts::{replay, study_result},
};
use crate::{
    AbandonCommand, AppError,
    app_error::{domain, study_input},
};
use tempo_store::{AnswerResult, Receipt, ReceiptResult, StoredAttempt, StudyScope, Write};

impl Tempo {
    /// Stop an attempt without producing a new assessment.
    ///
    /// Abandonment clears its active pointer and saves a receipt atomically. An
    /// assessment already emitted by an earlier miss remains part of the attempt.
    pub fn abandon(&mut self, command: AbandonCommand) -> Result<AnswerResult, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.abandon.abandon");
        self.leave(command, false)
    }

    pub(super) fn skip(&mut self, command: AbandonCommand) -> Result<AnswerResult, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.abandon.skip");
        self.leave(command, true)
    }

    fn leave(&mut self, command: AbandonCommand, skip: bool) -> Result<AnswerResult, AppError> {
        let scope = StudyScope {
            attempt: Some(&command.attempt_id),
            submission: Some(&command.submission_id),
            ..StudyScope::default()
        };
        let snapshot = self.store.study_state(scope)?;
        let action = if skip { "skip" } else { "abandon" };
        let fingerprint =
            serde_json::to_string(&(action, &command)).map_err(|e| domain("codec_error", e))?;
        if let Some(result) = replay(&snapshot, &command.submission_id, &fingerprint)? {
            return study_result(result);
        }
        let old = self.current(&snapshot, &command.dataset_id, &command.attempt_id)?;
        let attempt = crate::diagnostics::measure("study.abandon", || {
            tempo_study::abandon(&old.attempt, command.expected_revision)
        })
        .map_err(study_input)?;
        let result = AnswerResult {
            attempt: attempt.clone(),
            feedback: None,
            assessment: None,
            learning_effect: None,
        };
        let mut writes = vec![
            Write::Progress(StoredAttempt {
                attempt,
                ..old.clone()
            }),
            Write::Receipt(Receipt {
                id: command.submission_id.clone(),
                command: fingerprint,
                result: ReceiptResult::Study(result.clone()),
            }),
        ];
        if snapshot.active_attempt.as_ref() == Some(&command.attempt_id) {
            writes.push(Write::Active(None));
        }
        let at = (skip && old.mode != tempo_store::ActivityMode::Practice)
            .then(|| self.components.clock.now());
        self.commit_study(&snapshot, writes, scope)?;
        if let (Some(at), Some(daily)) = (at, &mut self.daily) {
            daily.queue.defer(result.attempt.target.clone(), at.day);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use crate::{AbandonCommand, Tempo};

    #[test]
    fn abandoning_clears_the_active_pointer_and_allows_a_fresh_attempt() {
        let mut app = Tempo::demo().unwrap();
        let issued = app.next_exercise().unwrap().unwrap();
        app.abandon(AbandonCommand {
            dataset_id: issued.attempt.dataset,
            attempt_id: issued.attempt.id.clone(),
            submission_id: "stop".into(),
            expected_revision: issued.attempt.revision,
        })
        .unwrap();
        assert_eq!(app.snapshot().unwrap().active_attempt, None);
        let replacement = app.next_exercise().unwrap().unwrap();
        assert_ne!(replacement.attempt.id, issued.attempt.id);
        assert_eq!(replacement.attempt.target, issued.attempt.target);
    }
}
