//! Stop an attempt and save the reply for safe retries.
use super::{
    Tempo,
    receipts::{replay_accepted_study, replay_study_in_scope},
};
use crate::{
    AbandonCommand, AppError,
    app_error::{domain, study_input},
};
use tempo_store::{
    ActivityMode, AnswerResult, Commit, Receipt, ReceiptResult, StoredAttempt, StudyScope,
    StudyState, Write,
};

use tempo_types::AttemptId;

pub(super) fn retired_warmup() -> AppError {
    AppError::new(
        "retired_activity",
        "Connected repertoire warm-ups have been retired.",
    )
}

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

    /// Next can retire previously issued Practice or a warm-up without grading it.
    pub(super) fn retire_legacy_active(
        &mut self,
        snapshot: &StudyState,
        id: &AttemptId,
    ) -> Result<bool, AppError> {
        if snapshot.active_attempt.as_ref() != Some(id) {
            return Ok(false);
        }
        let Some(record) = snapshot
            .attempts
            .get(id)
            .filter(|record| record.repertoire_warmup || record.mode == ActivityMode::Practice)
        else {
            return Ok(false);
        };
        let attempt =
            tempo_study::abandon(&record.attempt, record.attempt.revision).map_err(study_input)?;
        self.commit_study(
            snapshot,
            vec![
                Write::Progress(StoredAttempt {
                    attempt,
                    ..record.clone()
                }),
                Write::Active(None),
            ],
            StudyScope::default(),
        )?;
        Ok(true)
    }

    fn leave(&mut self, command: AbandonCommand, skip: bool) -> Result<AnswerResult, AppError> {
        let scope = StudyScope {
            attempt: Some(&command.attempt_id),
            submission: Some(&command.submission_id),
            ..StudyScope::default()
        };
        let action = if skip { "skip" } else { "abandon" };
        let fingerprint =
            serde_json::to_string(&(action, &command)).map_err(|e| domain("codec_error", e))?;
        if let Some(result) = replay_accepted_study(
            self.store.as_ref(),
            &command.dataset_id,
            &command.submission_id,
            &fingerprint,
        )? {
            return Ok(result);
        }
        // Recheck the receipt in the coherent new-work read. Another delivery
        // may have committed it since the addressed lookup.
        let snapshot = self.store.study_state(scope)?;
        if let Some(result) = replay_study_in_scope(
            &snapshot,
            &command.dataset_id,
            &command.submission_id,
            &fingerprint,
        )? {
            return Ok(result);
        }
        let old = self.current(&snapshot, &command.dataset_id, &command.attempt_id)?;
        if old.repertoire_warmup {
            return Err(retired_warmup());
        }
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
        let at = skip.then(|| self.components.clock.now());
        if let Some(at) = at.filter(|_| old.issued_work.is_some()) {
            self.store.commit_issued_skip(
                Commit {
                    expected_dataset: snapshot.dataset.clone(),
                    expected_revision: snapshot.revision,
                    writes,
                },
                scope,
                at.day,
            )?;
        } else {
            self.commit_study(&snapshot, writes, scope)?;
            // This is the unchanged legacy path for attempts issued before an
            // exact token is available. The prepared Next cutover removes it.
            if let (Some(at), Some(daily)) = (at, &mut self.daily) {
                daily.queue.defer(result.attempt.target.clone(), at.day);
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use crate::AbandonCommand;

    #[test]
    fn abandoning_clears_the_active_pointer_and_allows_a_fresh_attempt() {
        let (mut app, _directory) = crate::components::test_study();
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
