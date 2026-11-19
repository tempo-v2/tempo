//! Compatibility boundary for tactic facts in the retained catalog encoding.
//! The Tempo 1 converter writes a validated pack envelope. Capture Save marks
//! an accepted personal capture. Other provenance entries are attribution.
use serde::Deserialize;
use tempo_chess::ContentItem;
use tempo_learning::LearningState;
use tempo_store::Snapshot;
use tempo_types::{StudyDay, TargetId};

const PERSONAL_CAPTURE: &str = "tempo:capture-save";
const LEGACY_PACK: &str = "tempo1:catalog-metadata:";

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum PackDifficulty {
    Easy,
    Medium,
    Hard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PackMembership<'a> {
    pub theme: &'a str,
    pub difficulty: PackDifficulty,
    pub ordinal: usize,
    pub position: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LegacyPackMember<'a> {
    motif: &'a str,
    difficulty: &'a str,
    deck_id: &'a str,
    deck_position: usize,
}

/// Keep the historical pack validation at ingress. Deck spelling has no role
/// once the theme, difficulty, pack ordinal and position have been decoded.
fn pack_memberships(provenance: &[String]) -> impl Iterator<Item = PackMembership<'_>> {
    provenance.iter().filter_map(|entry| {
        let member: LegacyPackMember<'_> =
            serde_json::from_str(entry.strip_prefix(LEGACY_PACK)?).ok()?;
        let difficulty = match member.difficulty {
            "easy" => PackDifficulty::Easy,
            "medium" => PackDifficulty::Medium,
            "hard" => PackDifficulty::Hard,
            _ => return None,
        };
        let (pack, ordinal) = member.deck_id.rsplit_once('-')?;
        let (theme, pack_difficulty) = pack.rsplit_once('-')?;
        let ordinal = ordinal.parse().ok()?;
        (theme == member.motif
            && pack_difficulty == member.difficulty
            && ordinal > 0
            && member.deck_position > 0)
            .then_some(PackMembership {
                theme,
                difficulty,
                ordinal,
                position: member.deck_position,
            })
    })
}

pub(super) struct TacticPackCandidate<'a> {
    pub target: &'a TargetId,
    pub memberships: PackMemberships<'a>,
    pub first_introduced: Option<StudyDay>,
    pub guided_introduction: bool,
}

/// Delays reading legacy membership until the selection policy needs it.
pub(super) struct PackMemberships<'a>(&'a [String]);

impl PackMemberships<'_> {
    pub(super) fn iter(&self) -> impl Iterator<Item = PackMembership<'_>> {
        pack_memberships(self.0)
    }
}

/// Adapt retained content and Learning to the inputs used by new-pack ordering.
pub(super) fn tactic_pack_candidates(
    snapshot: &Snapshot,
) -> impl Iterator<Item = TacticPackCandidate<'_>> + Clone {
    snapshot
        .catalog
        .items
        .values()
        .filter(|item| item.active() && !item.definition.is_repertoire())
        .map(move |item| {
            let state = &snapshot.learning[&item.target];
            TacticPackCandidate {
                target: &item.target,
                memberships: PackMemberships(&item.provenance),
                first_introduced: state.first_introduced,
                guided_introduction: state.taught_at.is_some(),
            }
        })
}

/// A personal capture gets the introduction timestamp as first-review priority.
/// Queue still decides whether it is due and eligible to be selected.
pub(super) fn capture_review_priority(item: &ContentItem, state: &LearningState) -> Option<u64> {
    let introduced = state.taught_at?;
    (state.last_review_at.is_none()
        && CaptureOrigin::read(&item.provenance) == CaptureOrigin::PersonalCapture)
        .then_some(introduced.unix_ms)
}
