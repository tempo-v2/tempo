//! Grade one answer and commit progress, learning, assessment and replay receipt together.
use super::{
    Tempo,
    receipts::{replay, study_result},
};
use crate::{
    AbandonCommand, AppError, SubmitCommand,
    app_error::{domain, study_input},
};
use tempo_chess::{ChessAnswer, ChessFeedback};
use tempo_learning::{AssessmentInput, AssessmentSource, DailyLearning};
use tempo_queue::OrderEvent;
use tempo_store::{
    AcceptanceKind, ActivityAcceptance, ActivityMode, AnswerResult, AssessmentKey,
    AssessmentRecord, Receipt, ReceiptResult, RevealedDecision, StoredAttempt, StudyScope, Write,
};
use tempo_study::AttemptStatus;
use tempo_types::Grade;

enum Response {
    Move(ChessAnswer),
    Reveal,
    Concede,
}

impl Tempo {
    /// Accept one answer, or return its previously committed result on retry.
    ///
    /// The grader decides correctness. A new terminal assessment is interpreted
    /// by Learning and saved in the same commit as the attempt and receipt.
    pub fn submit(&mut self, command: SubmitCommand) -> Result<AnswerResult, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.submit.submit");
        let fingerprint =
            serde_json::to_string(&("answer", &command)).map_err(|e| domain("codec_error", e))?;
        let address = AbandonCommand {
            dataset_id: command.dataset_id,
            attempt_id: command.attempt_id,
            expected_revision: command.expected_revision,
            submission_id: command.submission_id,
        };
        self.advance(address, Response::Move(command.answer), fingerprint)
    }

    pub(super) fn give_up(&mut self, command: AbandonCommand) -> Result<AnswerResult, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.submit.give_up");
        // Installed releases used a blank-answer payload for the reveal fingerprint.
        let legacy = SubmitCommand {
            dataset_id: command.dataset_id.clone(),
            attempt_id: command.attempt_id.clone(),
            expected_revision: command.expected_revision,
            submission_id: command.submission_id.clone(),
            answer: "".into(),
        };
        let fingerprint =
            serde_json::to_string(&("give_up", legacy)).map_err(|e| domain("codec_error", e))?;
        self.advance(command, Response::Reveal, fingerprint)
    }

    pub(super) fn concede(&mut self, command: AbandonCommand) -> Result<AnswerResult, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.submit.concede");
        let fingerprint =
            serde_json::to_string(&("concede", &command)).map_err(|e| domain("codec_error", e))?;
        self.advance(command, Response::Concede, fingerprint)
    }

    fn advance(
        &mut self,
        command: AbandonCommand,
        response: Response,
        fingerprint: String,
    ) -> Result<AnswerResult, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.submit.advance");
        let scope = StudyScope {
            attempt: Some(&command.attempt_id),
            submission: Some(&command.submission_id),
            ..StudyScope::default()
        };
        let snapshot = self.store.study_state(scope)?;
        if let Some(result) = replay(&snapshot, &command.submission_id, &fingerprint)? {
            return study_result(result);
        }
        let old = self.current(&snapshot, &command.dataset_id, &command.attempt_id)?;
        if old.repertoire_warmup {
            return Err(super::abandon::retired_warmup());
        }
        let (item, prior) = snapshot
            .target(&old.attempt.target)?
            .ok_or_else(|| AppError::new("stale_content", "target is unavailable"))?;
        let definition = &item.definition;
        if matches!(old.mode, ActivityMode::Teach | ActivityMode::Repair) {
            return Err(AppError::new(
                "wrong_activity",
                "acknowledge this lesson after reproducing the shown move",
            ));
        }
        if matches!(response, Response::Concede) && old.mode != ActivityMode::Test {
            return Err(AppError::new(
                "wrong_activity",
                "Only a scheduled test can be conceded.",
            ));
        }
        let reveal = || ChessFeedback {
            correct: false,
            message: "The prescribed move is shown.".into(),
            autoplay: vec![],
        };
        let progress = match &response {
            Response::Concede => crate::diagnostics::measure("study.concede", || {
                tempo_study::concede(&old.attempt, command.expected_revision, reveal())
            }),
            Response::Reveal => crate::diagnostics::measure("study.give_up", || {
                tempo_study::give_up(&old.attempt, command.expected_revision, reveal())
            }),
            Response::Move(answer) => crate::diagnostics::measure("study.submit", || {
                tempo_study::submit(
                    self.components.grader.as_ref(),
                    definition,
                    &old.attempt,
                    command.expected_revision,
                    answer,
                )
            }),
        }
        .map_err(study_input)?;
        let accepted_at = progress.assessment.map(|_| self.components.clock.now());
        let acceptance = if self.daily.is_some() {
            accepted_at
                .zip(progress.assessment)
                .map(|(at, grade)| {
                    let decision_index = old.attempt.state.decision_index;
                    let kind = match (grade, &response) {
                        (Grade::Good, _) => AcceptanceKind::Recall,
                        (Grade::Again, Response::Move(answer)) => AcceptanceKind::WrongMove {
                            uci: answer.uci.clone(),
                            decision_index,
                        },
                        (Grade::Again, _) => AcceptanceKind::Revealed { decision_index },
                    };
                    ActivityAcceptance { at, kind }
                })
                .or_else(|| old.acceptance.clone())
        } else {
            old.acceptance.clone()
        };
        // Incorrect feedback authorizes the issued response at this cursor.
        // Concession also yields that transition before its inline reply is suppressed.
        let revealed_decision = (!progress.feedback.correct)
            .then_some(RevealedDecision {
                decision_index: old.attempt.state.decision_index,
            })
            .or(old.revealed_decision);
        let updated = StoredAttempt {
            attempt: progress.attempt.clone(),
            acceptance,
            revealed_decision,
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
                accepted_at: accepted_at.expect("new grade has acceptance time"),
                source: AssessmentSource::Human {
                    attempt_id: command.attempt_id.clone(),
                    issued_miss_revision: old.issued_miss_revision,
                },
            };
            let policy = self.components.learning.as_ref();
            let transition = if self.daily.is_some() {
                crate::diagnostics::measure("learning.assess_test", || {
                    DailyLearning::assess_test(
                        policy,
                        prior,
                        &input,
                        old.attempt.content_version,
                        old.issued_at,
                    )
                })
            } else {
                policy.apply(prior, &input)
            }
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
        if progress.assessment.is_none()
            && progress.attempt.status == AttemptStatus::Complete
            && let Some(record) = snapshot.assessments.iter().find(|record| {
                record.key == AssessmentKey::Human(command.attempt_id.clone())
                    && record.input.grade == Grade::Again
            })
        {
            if record.transition.state.correction.is_some() {
                let state = crate::diagnostics::measure("learning.repair", || {
                    DailyLearning::repair(
                        prior,
                        old.attempt.content_version,
                        record.transition.state.miss_revision,
                        self.components.clock.now(),
                    )
                })
                .map_err(|error| domain("learning_error", error))?;
                if state != *prior {
                    writes.push(Write::Learning {
                        target: old.attempt.target.clone(),
                        state,
                    });
                }
            }
        }
        let result = AnswerResult {
            attempt: progress.attempt,
            feedback: (!matches!(response, Response::Concede)).then_some(progress.feedback),
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
