//! Capture attribution and interim first-review priority.

use tempo_chess::ContentItem;
use tempo_learning::LearningState;

const PERSONAL_CAPTURE: &str = "tempo:capture-save";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CaptureOrigin {
    PersonalCapture,
    Other,
}

impl CaptureOrigin {
    pub(super) fn read(provenance: &[String]) -> Self {
        if provenance.iter().any(|entry| entry == PERSONAL_CAPTURE) {
            Self::PersonalCapture
        } else {
            Self::Other
        }
    }

    pub(super) fn record_personal_capture(provenance: &mut Vec<String>) {
        if Self::read(provenance) != Self::PersonalCapture {
            provenance.push(PERSONAL_CAPTURE.into());
            provenance.sort();
        }
    }
}

/// A personal capture gets the introduction timestamp as first-review priority.
/// Queue still decides whether it is due and eligible to be selected.
pub(super) fn capture_review_priority(item: &ContentItem, state: &LearningState) -> Option<u64> {
    let introduced = state.taught_at?;
    (state.last_review_at.is_none()
        && CaptureOrigin::read(&item.provenance) == CaptureOrigin::PersonalCapture)
        .then_some(introduced.unix_ms)
}

/// Tactic Capture stores the immutable PGN archive under this digest and
/// attaches that exact key to the imported target as source provenance.
pub(crate) fn saved_pgn_source(source: &str) -> bool {
    source
        .strip_prefix("pgn:")
        .is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|c| c.is_ascii_hexdigit()))
}
