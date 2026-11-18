//! Grade one answer and commit progress, learning, assessment and replay receipt together.
use super::{
    Tempo,
    receipts::{replay, study_result},
};
use crate::{
    AbandonCommand, AppError, SubmitCommand,
    app_error::{domain, study_input},
};
use tempo_chess::ChessFeedback;
use tempo_learning::{AssessmentInput, AssessmentSource};
use tempo_queue::OrderEvent;
use tempo_store::{
    ActivityMode, AnswerResult, AssessmentKey, AssessmentRecord, Receipt, ReceiptResult,
    StoredAttempt, StudyScope, Write,
};
use tempo_study::AttemptStatus;
use tempo_types::Grade;

impl Tempo {
    /// Accept one answer, or return its previously committed result on retry.
    ///
    /// The grader decides correctness. A new terminal assessment is interpreted
    /// by Learning and saved in the same commit as the attempt and receipt.
    pub fn submit(&mut self, command: SubmitCommand) -> Result<AnswerResult, AppError> {
        self.advance(command, false)
    }

    pub(super) fn give_up(&mut self, command: AbandonCommand) -> Result<AnswerResult, AppError> {
        self.advance(
            SubmitCommand {
                dataset_id: command.dataset_id,
                attempt_id: command.attempt_id,
                expected_revision: command.expected_revision,
                submission_id: command.submission_id.clone(),
                answer: "".into(),
            },
            true,
        )
    }

    fn advance(&mut self, command: SubmitCommand, give_up: bool) -> Result<AnswerResult, AppError> {
        let scope = StudyScope {
            attempt: Some(&command.attempt_id),
            submission: Some(&command.submission_id),
            ..StudyScope::default()
        };
        let snapshot = self.store.study_state(scope)?;
        let action = if give_up { "give_up" } else { "answer" };
        let fingerprint =
            serde_json::to_string(&(action, &command)).map_err(|e| domain("codec_error", e))?;
        if let Some(result) = replay(&snapshot, &command.submission_id, &fingerprint)? {
            return study_result(result);
        }
        let old = self.current(&snapshot, &command.dataset_id, &command.attempt_id)?;
        let definition = &snapshot.catalog.items[&old.attempt.target].definition;
        if old.mode == ActivityMode::Teach {
            return Err(AppError::new(
                "wrong_activity",
                "acknowledge this lesson after reproducing the shown move",
            ));
        }
        let progress = if give_up {
            tempo_study::give_up(
                &old.attempt,
                command.expected_revision,
                ChessFeedback {
                    correct: false,
                    message: "The prescribed move is shown.".into(),
                    autoplay: vec![],
                },
            )
        } else {
            tempo_study::submit(
                self.components.grader.as_ref(),
                definition,
                &old.attempt,
                command.expected_revision,
                &command.answer,
            )
        }
        .map_err(study_input)?;
        let updated = StoredAttempt {
            attempt: progress.attempt.clone(),
            ..old.clone()
        };
        let mut writes = vec![Write::Progress(updated)];
        let mut effect = None;
        if let Some(grade) = progress
            .assessment
            .filter(|_| old.mode == ActivityMode::Test)
        {
            let input = AssessmentInput {
                grade,
                accepted_at: self.components.clock.now(),
                source: AssessmentSource::Human {
                    attempt_id: command.attempt_id.clone(),
                    issued_miss_revision: old.issued_miss_revision,
                },
            };
            let transition = self
                .components
                .learning
                .apply(&snapshot.learning[&old.attempt.target], &input)
                .map_err(|e| domain("learning_error", e))?;
            effect = Some(transition.effect);
            writes.push(Write::Learning {
                target: old.attempt.target.clone(),
                state: transition.state.clone(),
            });
            writes.push(Write::Assessment(AssessmentRecord {
                key: AssessmentKey::Human(command.attempt_id.clone()),
                dataset: snapshot.dataset.clone(),
                target: old.attempt.target.clone(),
                content_version: old.attempt.content_version,
                input,
                transition,
            }));
        }
        let result = AnswerResult {
            attempt: progress.attempt,
            feedback: Some(progress.feedback),
            assessment: progress
                .assessment
                .filter(|_| old.mode == ActivityMode::Test),
            learning_effect: effect,
        };
        writes.push(Write::Receipt(Receipt {
            id: command.submission_id.clone(),
            command: fingerprint,
            result: ReceiptResult::Study(result.clone()),
        }));
        if result.attempt.status != AttemptStatus::Active
            && snapshot.active_attempt.as_ref() == Some(&result.attempt.id)
        {
            writes.push(Write::Active(None));
        }
        self.commit_study(&snapshot, writes, scope)?;
        if self.daily.is_none() && result.assessment == Some(Grade::Again) {
            self.order_history.push(OrderEvent::Missed {
                target: result.attempt.target.clone(),
            });
        }
        if self.daily.is_none() && result.attempt.status == AttemptStatus::Complete {
            self.order_history.push(OrderEvent::Completed {
                target: result.attempt.target.clone(),
            });
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use crate::{SubmitCommand, Tempo};
    use tempo_types::Grade;

    #[test]
    fn an_accepted_miss_saves_progress_learning_assessment_and_receipt_together() {
        let mut app = Tempo::demo().unwrap();
        let issued = app.next_exercise().unwrap().unwrap();
        let result = app
            .submit(SubmitCommand {
                dataset_id: issued.attempt.dataset,
                attempt_id: issued.attempt.id,
                submission_id: "miss-action".into(),
                expected_revision: issued.attempt.revision,
                answer: "e2e3".into(),
            })
            .unwrap();
        let saved = app.snapshot().unwrap();
        assert_eq!(result.assessment, Some(Grade::Again));
        assert_eq!(saved.attempts[&result.attempt.id].attempt, result.attempt);
        assert!(saved.learning[&result.attempt.target].repeat_pending);
        assert_eq!(saved.assessments.len(), 1);
        assert_eq!(saved.receipts.len(), 1);
    }
}
