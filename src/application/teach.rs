//! Accept guided reproduction as instruction, with no graded Study submission.
use super::{
    Tempo,
    receipts::{replay, study_result},
};
use crate::{
    AppError, SubmitCommand,
    app_error::{domain, study_input},
};
use tempo_learning::DailyLearning;
use tempo_store::{
    AcceptanceKind, ActivityAcceptance, ActivityMode, AnswerResult, Receipt, ReceiptResult,
    StoredAttempt, StudyScope, Write,
};
use tempo_study::{AttemptStatus, check_open::check_open};

impl Tempo {
    pub(super) fn acknowledge(&mut self, command: SubmitCommand) -> Result<AnswerResult, AppError> {
        let scope = StudyScope {
            attempt: Some(&command.attempt_id),
            submission: Some(&command.submission_id),
            ..StudyScope::default()
        };
        let snapshot = self.store.study_state(scope)?;
        let fingerprint = serde_json::to_string(&("acknowledge", &command))
            .map_err(|error| domain("codec_error", error))?;
        if let Some(result) = replay(&snapshot, &command.submission_id, &fingerprint)? {
            return study_result(result);
        }
        let old = self.current(&snapshot, &command.dataset_id, &command.attempt_id)?;
        if old.mode != ActivityMode::Teach {
            return Err(AppError::new(
                "wrong_activity",
                "This activity is a blind test.",
            ));
        }
        let revision = check_open(&old.attempt, command.expected_revision).map_err(study_input)?;
        let item = &snapshot.catalog.items[&old.attempt.target];
        if item.definition.decisions[0].expected != command.answer {
            return Err(AppError::new(
                "try_shown_move",
                "Reproduce the shown move before acknowledging the lesson.",
            ));
        }
        let at = self.components.clock.now();
        let state = DailyLearning::teach(&snapshot.learning[&item.target], at)
            .map_err(|error| domain("learning_error", error))?;
        let mut attempt = old.attempt.clone();
        attempt.revision = revision;
        attempt.state.decision_index = 1;
        attempt.status = AttemptStatus::Complete;
        attempt.prompt = None;
        let reply = AnswerResult {
            attempt: attempt.clone(),
            feedback: None,
            assessment: None,
            learning_effect: None,
        };
        self.commit_study(
            &snapshot,
            vec![
                Write::Progress(StoredAttempt {
                    attempt,
                    acceptance: Some(ActivityAcceptance {
                        at,
                        kind: AcceptanceKind::Teaching,
                    }),
                    ..old.clone()
                }),
                Write::Learning {
                    target: item.target.clone(),
                    state,
                },
                Write::Active(None),
                Write::Receipt(Receipt {
                    id: command.submission_id.clone(),
                    command: fingerprint,
                    result: ReceiptResult::Study(reply.clone()),
                }),
            ],
            scope,
        )?;
        Ok(reply)
    }
}
