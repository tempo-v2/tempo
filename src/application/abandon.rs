//! Stop an attempt and save the reply for safe retries.
use super::{
    Tempo,
    receipts::{replay, study_result},
};
use crate::{
    AbandonCommand, AppError,
    app_error::{domain, study_input},
};
use tempo_store::{AnswerResult, Receipt, ReceiptResult, StoredAttempt, Write};

impl Tempo {
    /// Stop an attempt without producing a new assessment.
    ///
    /// Abandonment clears its active pointer and saves a receipt atomically. An
    /// assessment already emitted by an earlier miss remains part of the attempt.
    pub fn abandon(&mut self, command: AbandonCommand) -> Result<AnswerResult, AppError> {
        let snapshot = self.store.snapshot()?;
        let fingerprint =
            serde_json::to_string(&("abandon", &command)).map_err(|e| domain("codec_error", e))?;
        if let Some(result) = replay(&snapshot, &command.submission_id, &fingerprint)? {
            return study_result(result);
        }
        let old = self.current(&snapshot, &command.dataset_id, &command.attempt_id)?;
        let attempt =
            tempo_study::abandon(&old.attempt, command.expected_revision).map_err(study_input)?;
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
                id: command.submission_id,
                command: fingerprint,
                result: ReceiptResult::Study(result.clone()),
            }),
        ];
        if snapshot.active_attempt.as_ref() == Some(&command.attempt_id) {
            writes.push(Write::Active(None));
        }
        self.commit(&snapshot, writes)?;
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
