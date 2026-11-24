use rand::Rng;
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub new: usize,
    pub reviews: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NewOpeningSettings {
    pub paused: bool,
    pub ordering: NewOpeningOrder,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NewOpeningOrder {
    #[serde(rename = "maia")]
    SavedRank,
    Standard,
}

impl NewOpeningOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SavedRank => "maia",
            Self::Standard => "standard",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum CardKind {
    Opening,
    Tactic,
}

impl CardKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Opening => "opening",
            Self::Tactic => "tactic",
        }
    }
}

/// Pausing new openings affects uncommitted candidates. Committed work is kept.
pub fn admits_uncommitted_new(kind: CardKind, openings_paused: bool) -> bool {
    kind != CardKind::Opening || !openings_paused
}

impl Counts {
    /// Add committed work absent from the indexed, unreserved candidate totals.
    pub fn include_committed(
        &mut self,
        kind: CardKind,
        due_at: Option<i64>,
        reserved: bool,
        openings_paused: bool,
        now: i64,
    ) {
        match due_at {
            None if reserved || !admits_uncommitted_new(kind, openings_paused) => self.new += 1,
            Some(due_at) if reserved || due_at > now => self.reviews += 1,
            _ => {}
        }
    }
}

pub enum Candidates {
    Due,
    NewByRank,
    New,
}

/// Call the store only for the next applicable candidate class.
pub fn select<T, E>(
    previous: Option<CardKind>,
    preferences: impl FnOnce() -> Result<NewOpeningSettings, E>,
    mut first: impl FnMut(CardKind, Candidates) -> Result<Option<T>, E>,
) -> Result<Option<(T, CardKind)>, E> {
    use CardKind::{Opening, Tactic};
    let kinds = if previous == Some(Opening) {
        [Tactic, Opening]
    } else {
        [Opening, Tactic]
    };
    for kind in kinds {
        if let Some(id) = first(kind, Candidates::Due)? {
            return Ok(Some((id, kind)));
        }
    }
    let preferences = preferences()?;
    for kind in kinds {
        if !admits_uncommitted_new(kind, preferences.paused) {
            continue;
        }
        if kind == Opening
            && matches!(preferences.ordering, NewOpeningOrder::SavedRank)
            && let Some(id) = first(kind, Candidates::NewByRank)?
        {
            return Ok(Some((id, kind)));
        }
        if let Some(id) = first(kind, Candidates::New)? {
            return Ok(Some((id, kind)));
        }
    }
    Ok(None)
}

pub fn draw_repeat_offset() -> u8 {
    rand::rng().random_range(5..=20)
}

pub fn repeat_offset(draw: u8, other_cards: usize) -> u8 {
    usize::from(draw).min(other_cards) as u8
}
