//! The current attempt as presentation data, with private grading state removed.
use serde::{Deserialize, Serialize};
use tempo_chess::{ChessFeedback, ChessPrompt};
use tempo_types::{AttemptId, ContentVersion, DatasetId, Grade, Revision, TargetId};

/// Render an issued attempt and copy its identity/revision into the next intent.
///
/// The application creates this from a persisted attempt result. The UI consumes
/// the prompt and feedback; it receives neither expected moves nor scheduler state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttemptView {
    /// Wire schema version, currently 1.
    pub schema_version: u32,
    /// `ready`, `complete`, or `abandoned` in version 1.
    pub status: String,
    /// Dataset identity that invalidates commands after a portable restore.
    pub dataset_id: DatasetId,
    /// This attempt, shared by all learner decisions within it.
    pub attempt_id: AttemptId,
    /// The content target whose learning history this attempt affects.
    pub target_id: TargetId,
    /// The semantic definition pinned when the attempt was issued.
    pub content_version: ContentVersion,
    /// Copy into `expected_revision` on the next answer or abandonment.
    pub revision: Revision,
    /// Present only while the attempt is ready for learner input.
    pub prompt: Option<ChessPrompt>,
    /// Feedback for this response; resume does not replay the last message.
    pub feedback: Option<ChessFeedback>,
    /// The attempt's lifetime grade, including an earlier miss during correction.
    pub assessment: Option<Grade>,
}

#[cfg(test)]
mod tests {
    use crate::{
        Tempo,
        wire::{StudyCommand, StudyView},
    };

    #[test]
    fn the_ui_gets_prompt_and_identity_but_no_private_answer_or_learning_state() {
        let mut app = Tempo::demo().unwrap();
        let StudyView::Attempt(view) = app.handle(StudyCommand::Next) else {
            panic!("the demo has a ready attempt");
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["revision"], 0);
        assert_eq!(json["status"], "ready");
        assert!(json["prompt"]["fen"].is_string());
        assert!(json["prompt"].get("expected").is_none());
        assert!(json.get("state").is_none());
        assert!(json.get("learning").is_none());
    }
}
