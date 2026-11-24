//! Answer one learner decision in the currently displayed attempt.
use serde::{Deserialize, Serialize};
use tempo_chess::ChessAnswer;
use tempo_types::{AttemptId, DatasetId, Revision, SubmissionId};

/// Answer one learner decision in the currently displayed attempt.
///
/// Copy dataset, attempt and expected revision from the latest application reply.
/// Allocate a submission ID once for this user action, then keep the *same whole
/// command* for retries. [`crate::Tempo::submit`] checks receipts before stale
/// content checks, so a lost reply can be recovered after an intervening edit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitCommand {
    /// Dataset from the displayed attempt.
    pub dataset_id: DatasetId,
    /// The attempt containing this learner decision.
    pub attempt_id: AttemptId,
    /// One user action identity, reused unchanged on retries.
    pub submission_id: SubmissionId,
    /// Attempt revision from the latest reply, not the store revision.
    pub expected_revision: Revision,
    /// Learner input only; correctness is decided by the injected grader.
    pub answer: ChessAnswer,
}

#[cfg(test)]
mod tests {
    use super::SubmitCommand;

    #[test]
    fn copy_the_displayed_revision_and_reuse_the_whole_command_after_a_lost_reply() {
        let (mut app, _directory) = crate::components::test_study();
        let issued = app.next_exercise().unwrap().unwrap();
        let command = SubmitCommand {
            dataset_id: issued.attempt.dataset,
            attempt_id: issued.attempt.id,
            submission_id: "answer-action-1".into(),
            expected_revision: issued.attempt.revision,
            answer: "e2e4".into(),
        };
        let first_reply = app.submit(command.clone()).unwrap();
        let committed_revision = app.snapshot().unwrap().revision;
        let recovered_reply = app.submit(command).unwrap();
        assert_eq!(recovered_reply, first_reply);
        assert_eq!(app.snapshot().unwrap().revision, committed_revision);
    }
}
