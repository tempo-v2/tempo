use serde::{Deserialize, Serialize};
use tempo_types::{AttemptId, ContentVersion, DatasetId, Revision, TargetId};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttemptView {
    pub schema_version: u32,
    pub status: String,
    pub dataset_id: DatasetId,
    pub attempt_id: AttemptId,
    pub target_id: TargetId,
    pub content_version: ContentVersion,
    pub revision: Revision,
    pub prompt: Option<ChessPrompt>,
    pub feedback: Option<ChessFeedback>,
    pub assessment: Option<Grade>,
}
