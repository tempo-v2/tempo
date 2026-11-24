use crate::chess;
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum Conflict {
    Dataset,
    Submission,
}

impl Conflict {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Dataset => "stale_dataset",
            Self::Submission => "answer_conflict",
        }
    }
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Dataset => "The Study dataset changed.",
            Self::Submission => "This card already has a different accepted answer.",
        })
    }
}
impl std::error::Error for Conflict {}

#[derive(Deserialize)]
pub struct Next {
    pub after: Option<i64>,
    pub dataset: Option<String>,
}

#[derive(Deserialize)]
pub struct Answer {
    pub attempt: i64,
    pub dataset: String,
    pub played: Option<String>,
}

#[derive(Serialize)]
pub struct Review {
    pub attempt: i64,
    pub dataset: String,
    pub kind: String,
    pub title: String,
    pub color: String,
    pub fen: String,
    pub legal: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lead_in: Option<chess::Route>,
    pub result: Option<AcceptedView>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Accepted {
    pub rating: String,
    pub repeat_after: Option<u8>,
    pub answers: Vec<String>,
    pub played_in_game: Option<String>,
}

// The receipt stays in its original format. Project the displayed move
// from the accepted submission, including for receipts from older releases.
#[derive(Serialize)]
pub struct AcceptedView {
    #[serde(flatten)]
    result: Accepted,
    display_move: Option<String>,
}

pub(crate) fn accepted_view(result: Accepted, fen: &str, played: Option<&str>) -> AcceptedView {
    let played = played
        .filter(|_| result.rating == "Good")
        .and_then(|played| {
            chess::position(fen)
                .ok()
                .and_then(|board| chess::parse_move(&board, played).ok())
        })
        .map(chess::uci)
        .filter(|played| result.answers.contains(played));
    let display_move = played.or_else(|| result.answers.first().cloned());
    AcceptedView {
        result,
        display_move,
    }
}

#[derive(Serialize)]
pub struct CardContext {
    pub game_id: Option<String>,
    pub ply: Option<usize>,
    pub played: Option<String>,
    pub lead_in: Option<chess::Route>,
}

pub enum NextState {
    Current,
    Select { previous_kind: Option<String> },
}

pub struct IssuedAnswer {
    pub card: String,
    pub fen: String,
    pub answers: Vec<String>,
    pub played_in_game: Option<String>,
    pub previous_kind: Option<String>,
    pub(crate) payload: String,
}

pub enum AnswerState {
    Replayed(AcceptedView),
    Pending(IssuedAnswer),
}

pub struct GameAnchor {
    pub game_id: String,
    pub ply: usize,
    pub fen: String,
}

pub struct ContextSource {
    pub position_id: String,
    pub repertoire_id: Option<String>,
    pub answer_version: Option<String>,
    pub context: CardContext,
}

pub(crate) fn check_dataset(provided: Option<&str>, stored: &str) -> Result<(), Conflict> {
    if provided == Some(stored) {
        Ok(())
    } else {
        Err(Conflict::Dataset)
    }
}

pub(crate) fn should_select(active: Option<i64>, accepted: bool, after: Option<i64>) -> bool {
    match active {
        Some(attempt) => accepted && after == Some(attempt),
        None => after.is_none(),
    }
}

pub(crate) fn verify_replay(original: Option<&str>, supplied: &str) -> Result<(), Conflict> {
    if original == Some(supplied) {
        Ok(())
    } else {
        Err(Conflict::Submission)
    }
}
