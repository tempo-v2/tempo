//! Stop an attempt without claiming an answer or assigning a new grade.
use serde::{Deserialize, Serialize};
use tempo_types::{AttemptId, DatasetId, Revision, SubmissionId};

/// Stop an attempt without claiming an answer or assigning a new grade.
///
/// The UI constructs this from its latest attempt view. As with an answer, a
/// fresh action gets one submission ID; retries reuse that ID and expected revision.
/// [`crate::Tempo::abandon`] persists the stopped attempt and replay receipt together.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbandonCommand {
    /// Dataset from the displayed attempt.
    pub dataset_id: DatasetId,
    /// Attempt to stop.
    pub attempt_id: AttemptId,
    /// One stop action identity, reused on retries.
    pub submission_id: SubmissionId,
    /// Attempt revision the user was viewing when they stopped.
    pub expected_revision: Revision,
}

#[cfg(test)]
mod tests {
    use super::AbandonCommand;
    use tempo_study::AttemptStatus;

    #[test]
    fn a_stop_intent_carries_concurrency_identity_but_no_answer_or_grade() {
        let (mut app, _directory) = crate::components::test_study();
        let issued = app.next_exercise().unwrap().unwrap();
        let command = AbandonCommand {
            dataset_id: issued.attempt.dataset,
            attempt_id: issued.attempt.id,
            submission_id: "stop-action-1".into(),
            expected_revision: issued.attempt.revision,
        };
        let wire = serde_json::to_value(&command).unwrap();
        assert!(wire.get("answer").is_none());
        assert!(wire.get("assessment").is_none());
        let stopped = app.abandon(serde_json::from_value(wire).unwrap()).unwrap();
        assert_eq!(stopped.attempt.status, AttemptStatus::Abandoned);
        assert_eq!(stopped.assessment, None);
    }
}
