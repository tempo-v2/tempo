use rand::Rng;
use serde::{Deserialize, Serialize};

pub const COUNT_CAP: usize = 10_000;

pub const MAX_DAILY_NEW: u8 = 20;

#[derive(Debug, Serialize)]
pub struct NewOpeningSettings {
    pub paused: bool,
    pub ordering: NewOpeningOrder,
    pub opening_limit: u8,
    pub tactic_limit: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateNewOpeningSettings {
    pub paused: bool,
    pub ordering: NewOpeningOrder,
    // Legacy Settings forms never supplied these fields.
    pub opening_limit: Option<u8>,
    pub tactic_limit: Option<u8>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NewOpeningOrder {
    Maia,
    Standard,
}

impl NewOpeningOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Maia => "maia",
            Self::Standard => "standard",
        }
    }
}

pub fn draw_repeat_offset() -> u8 {
    rand::rng().random_range(5..=20)
}

pub fn repeat_offset(draw: u8, other_cards: usize) -> u8 {
    usize::from(draw).min(other_cards) as u8
}
