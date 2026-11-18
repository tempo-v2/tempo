//! Record an identified external miss against the content that was compared.
use serde::{Deserialize, Serialize};
use tempo_types::{ContentVersion, DatasetId, EventId, TargetId};

/// Record an identified external miss against the content that was compared.
///
/// Game ingestion or another feedback adapter constructs this; it does not create
/// a human attempt. `expected_content_version` prevents applying old analysis to
/// an edited answer. Provider plus event ID identify a replayable event within the
/// dataset. [`crate::Tempo::again`] consumes it and commits the learning transition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgainCommand {
    /// Dataset whose repertoire was compared.
    pub dataset_id: DatasetId,
    /// The learning target that the observation missed.
    pub target: TargetId,
    /// Semantic version used by the comparison.
    pub expected_content_version: ContentVersion,
    /// Stable namespace for the source of feedback.
    pub provider: String,
    /// Stable identity for this observation within the provider.
    pub event_id: EventId,
    /// When the source observed the miss; acceptance time comes from Clock.
    pub observed_at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::AgainCommand;
    use crate::Tempo;
    use tempo_types::ContentVersion;

    #[test]
    fn feedback_must_name_the_definition_that_was_actually_compared() {
        let mut app = Tempo::demo().unwrap();
        let before = app.snapshot().unwrap();
        let stale = AgainCommand {
            dataset_id: before.dataset.clone(),
            target: "r1:p1".into(),
            expected_content_version: ContentVersion(99),
            provider: "games".into(),
            event_id: "game-42-position-1".into(),
            observed_at_ms: 5,
        };
        assert_eq!(app.again(stale).unwrap_err().code, "stale_content");
        assert_eq!(app.snapshot().unwrap(), before);
    }
}
