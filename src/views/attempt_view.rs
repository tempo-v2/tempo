//! The current attempt as presentation data, with private grading state removed.
use serde::{Deserialize, Serialize};
use tempo_chess::{ChessFeedback, ChessPrompt, LearnerColor, TacticExplanation};
use tempo_store::ActivityMode;
use tempo_types::{AttemptId, ContentVersion, DatasetId, Grade, Revision, TargetId};

/// Render an issued attempt and copy its identity/revision into the next intent.
///
/// The application creates this from a persisted attempt result. The UI consumes
/// the prompt and per-answer feedback. Only a miss reveals a correction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttemptView {
    /// Wire schema version, currently 5.
    pub schema_version: u32,
    /// `ready`, `complete`, or `abandoned`.
    pub status: String,
    pub activity: ActivityMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corrective_repair: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lesson: Option<LessonView>,
    /// The opponent move into this board, when the issued line establishes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opponent_move: Option<OpponentMoveView>,
    /// Presentation for the optional Practice before a scheduled repertoire review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repertoire_context: Option<RepertoirePresentation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional public display text. Consumers may render or omit it.
    pub exercise_name: Option<String>,
    /// Played moves only, from this response's issued version and cursor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<tempo_chess::RevealedLine>,
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
    /// Authoritative board, including any replies already played.
    pub display_fen: String,
    /// Fixed from the exercise's solver-start position.
    pub learner_color: LearnerColor,
    /// All legal moves on a ready board. Terminal boards accept no moves.
    pub legal_moves: Vec<String>,
    /// Whether the active side is in check on this board.
    pub in_check: bool,
    /// Present only while the attempt is ready for learner input.
    pub prompt: Option<ChessPrompt>,
    /// Feedback for this response; resume does not replay the last message.
    pub feedback: Option<AttemptFeedback>,
    /// The attempt's lifetime grade, including an earlier miss during correction.
    pub assessment: Option<Grade>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepertoirePresentation {
    Warmup,
}

/// Display-only context projected for the current board, including on resume.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpponentMoveView {
    pub uci: String,
    /// Supplied only on an unplayed first decision to authorize the opening hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_fen: Option<String>,
}

/// Guided reproduction data. Blind tests never include a lesson.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LessonView {
    pub move_uci: String,
    pub move_san: String,
    pub comments: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_move: Option<String>,
}

/// Presentation-only feedback. Stored submission receipts use ChessFeedback.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttemptFeedback {
    #[serde(flatten)]
    pub result: ChessFeedback,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correction_uci: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correction_san: Option<String>,
    /// Verified board relationships for an already revealed move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tactic_explanation: Option<TacticExplanation>,
}

pub(crate) struct CorrectionView {
    pub uci: String,
    pub san: String,
    pub tactic_explanation: Option<TacticExplanation>,
}

#[cfg(test)]
mod tests {
    use crate::{
        Tempo,
        wire::{StudyCommand, StudyView},
    };

    #[test]
    fn only_the_miss_discloses_the_chess_projected_correction_label() {
        use crate::SubmitCommand;

        let mut app = Tempo::demo().unwrap();
        let StudyView::Attempt(issued) = app.handle(StudyCommand::Next) else {
            panic!("ready exercise");
        };
        assert!(issued.feedback.is_none());
        let miss = app.handle(StudyCommand::Answer(SubmitCommand {
            dataset_id: issued.dataset_id,
            attempt_id: issued.attempt_id,
            expected_revision: issued.revision,
            submission_id: "show-san".into(),
            answer: "e2e3".into(),
        }));
        let json = serde_json::to_value(&miss).unwrap();
        assert_eq!(json["feedback"]["correction_san"], "e4");
        let legacy = serde_json::to_value(miss.without_move_details()).unwrap();
        assert_eq!(legacy["feedback"]["correction_uci"], "e2e4");
        assert!(legacy["feedback"].get("correction_san").is_none());
    }

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
