//! Accept guided reproduction as instruction, with no graded Study submission.
use super::{
    Tempo,
    receipts::{replay_accepted_study, replay_study_in_scope},
};
use crate::{
    AppError, SubmitCommand,
    app_error::{domain, study_input},
};
use tempo_chess::{GuidedMoveError, acknowledge_shown_move};
use tempo_learning::DailyLearning;
use tempo_store::{
    AcceptanceKind, ActivityAcceptance, ActivityMode, AnswerResult, Receipt, ReceiptResult,
    StoredAttempt, StudyScope, Write,
};
use tempo_study::{check_open::check_open, complete_ungraded};

impl Tempo {
    pub(super) fn acknowledge(&mut self, command: SubmitCommand) -> Result<AnswerResult, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.teach.acknowledge");
        let scope = StudyScope {
            attempt: Some(&command.attempt_id),
            submission: Some(&command.submission_id),
            ..StudyScope::default()
        };
        let fingerprint = serde_json::to_string(&("acknowledge", &command))
            .map_err(|error| domain("codec_error", error))?;
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
        if !matches!(old.mode, ActivityMode::Teach | ActivityMode::Repair) {
            return Err(AppError::new(
                "wrong_activity",
                "This activity is a blind test.",
            ));
        }
        crate::diagnostics::measure("study.check_open", || {
            check_open(&old.attempt, command.expected_revision)
        })
        .map_err(study_input)?;
        let (item, prior) = snapshot
            .target(&old.attempt.target)?
            .ok_or_else(|| AppError::new("stale_content", "target is unavailable"))?;
        let next = acknowledge_shown_move(&item.definition, &old.attempt.state, &command.answer)
            .map_err(|error| match error {
                GuidedMoveError::TryShownMove => AppError::new(
                    "try_shown_move",
                    "Reproduce the shown move before acknowledging the lesson.",
                ),
                _ => AppError::new("invalid_content", "The shown move is unavailable."),
            })?;
        let attempt = crate::diagnostics::measure("study.complete_ungraded", || {
            complete_ungraded(&old.attempt, command.expected_revision, next)
        })
        .map_err(study_input)?;
        let at = self.components.clock.now();
        let state = match old.mode {
            ActivityMode::Repair if old.issued_work.is_some() => Ok(
                DailyLearning::complete_human_correction(prior, item.version, old.issued_miss_revision),
            ),
            ActivityMode::Repair => crate::diagnostics::measure("learning.repair", || {
                DailyLearning::repair(prior, item.version, old.issued_miss_revision, at)
            }),
            _ if prior.miss_revision != old.issued_miss_revision => Ok(prior.clone()),
            _ => crate::diagnostics::measure("learning.teach", || DailyLearning::teach(prior, at)),
        }
        .map_err(|error| domain("learning_error", error))?;
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
