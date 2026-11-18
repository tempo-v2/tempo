//! Content and learning data that can move to an empty destination dataset.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tempo_chess::Catalog;
use tempo_learning::{LearningState, SchedulerSettings};
use tempo_types::TargetId;

/// The export/restore payload consumed by [`crate::SnapshotCodec`].
///
/// Reach for this in an interchange adapter. It includes policy settings so
/// learning memory can be checked against the chosen implementation. It omits
/// attempt history, active pointers, receipts and the source dataset identity;
/// restoration creates a fresh identity in an empty destination.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableSnapshot {
    pub schema_version: u32,
    pub catalog: Catalog,
    pub learning: BTreeMap<TargetId, LearningState>,
    pub policy: SchedulerSettings,
    #[serde(flatten)]
    pub game_evidence: tempo_store::PortableGameEvidence,
}

#[cfg(test)]
mod tests {
    use crate::{JsonSnapshotCodec, SnapshotCodec, Tempo};

    #[test]
    fn portability_carries_content_and_learning_without_live_attempt_identity() {
        let mut app = Tempo::demo().unwrap();
        app.next_exercise().unwrap();
        let text = app.export(&JsonSnapshotCodec).unwrap();
        let portable = JsonSnapshotCodec.decode(&text).unwrap();
        assert_eq!(portable.learning, app.snapshot().unwrap().learning);
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert!(json.get("dataset").is_none());
        assert!(json.get("attempts").is_none());
        assert!(json.get("receipts").is_none());
    }
}
