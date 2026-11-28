use crate::{chess, learning::Rating};
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum Conflict {
    Dataset,
    Submission,
    Attempt,
    PackChanged,
    IllegalMove,
    FullSolution,
}

impl Conflict {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Dataset => "stale_dataset",
            Self::Submission => "answer_conflict",
            Self::Attempt => "stale_attempt",
            Self::PackChanged => "pack_changed",
            Self::IllegalMove => "illegal_move",
            Self::FullSolution => "refresh_required",
        }
    }
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Dataset => "The Study dataset changed.",
            Self::Submission => "This card already has a different accepted answer.",
            Self::Attempt => "This attempt is no longer available.",
            Self::PackChanged => "The puzzle catalog changed. Reload the pack.",
            Self::IllegalMove => "That move is illegal in this position.",
            Self::FullSolution => "Reload Tempo to continue the full puzzle.",
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
pub struct AnswerWire {
    attempt: i64,
    dataset: String,
    // The predecessor browser sends null for an explicit reveal.
    #[serde(deserialize_with = "Deserialize::deserialize")]
    played: Option<String>,
}

impl From<AnswerWire> for AnswerIntentV1 {
    fn from(wire: AnswerWire) -> Self {
        Self {
            attempt: wire.attempt,
            dataset: wire.dataset,
            action: wire.played.map_or(AnswerAction::Reveal, AnswerAction::Play),
        }
    }
}

pub struct AnswerIntentV1 {
    pub attempt: i64,
    pub dataset: String,
    pub action: AnswerAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnswerAction {
    Play(String),
    Reveal,
}

impl AnswerAction {
    pub fn played(&self) -> Option<&str> {
        match self {
            Self::Play(value) => Some(value),
            Self::Reveal => None,
        }
    }

    pub fn canonical(&self, fen: &str) -> anyhow::Result<Self> {
        match self {
            Self::Play(value) => Ok(Self::Play(chess::uci(chess::parse_move(
                &chess::position(fen)?,
                value,
            )?))),
            Self::Reveal => Ok(Self::Reveal),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum RepeatDisposition {
    None,
    Placed(u8),
    Deferred,
}

#[derive(Serialize, Deserialize)]
pub struct AnswerResultV1 {
    pub action: AnswerAction,
    pub rating: Rating,
    pub repeat: RepeatDisposition,
    pub answers: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnswerReceipt {
    Versioned { v1: AnswerResultV1 },
    Legacy(Accepted),
}

impl AnswerReceipt {
    pub(crate) fn answers(&self) -> &[String] {
        match self {
            Self::Versioned { v1 } => &v1.answers,
            Self::Legacy(result) => &result.answers,
        }
    }

    pub fn view(
        self,
        fen: &str,
        original: Option<&str>,
        played_in_game: Option<String>,
    ) -> anyhow::Result<AcceptedView> {
        let (result, played) = match self {
            Self::Versioned { v1 } => {
                let played = v1.action.played().map(str::to_owned);
                let repeat_after = match v1.repeat {
                    RepeatDisposition::Placed(offset) => Some(offset),
                    RepeatDisposition::None | RepeatDisposition::Deferred => None,
                };
                (
                    Accepted {
                        rating: v1.rating.label().into(),
                        repeat_after,
                        answers: v1.answers,
                        played_in_game,
                    },
                    played,
                )
            }
            Self::Legacy(result) => {
                let played = original
                    .map(serde_json::from_str::<Option<String>>)
                    .transpose()?
                    .flatten();
                (result, played)
            }
        };
        Ok(accepted_view(result, fen, played.as_deref()))
    }

    pub fn verify(
        &self,
        fen: &str,
        original: Option<&str>,
        action: &AnswerAction,
    ) -> anyhow::Result<()> {
        let matches = match self {
            Self::Versioned { v1 } => action
                .canonical(fen)
                .is_ok_and(|supplied| supplied == v1.action),
            Self::Legacy(_) => original == Some(&serde_json::to_string(&action.played())?),
        };
        anyhow::ensure!(matches, Conflict::Submission);
        Ok(())
    }
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
    #[serde(flatten)]
    pub activity: Activity,
}

#[derive(Serialize)]
#[serde(tag = "activity_mode", rename_all = "snake_case")]
pub enum Activity {
    SingleAnswer {
        result: Option<AcceptedView>,
        /// Authorized authored move for an unanswered first opening exposure.
        #[serde(skip_serializing_if = "Option::is_none")]
        introduction_move: Option<String>,
    },
    FullLine {
        // Preserve the top-level null expected by browsers from the prior release.
        result: (),
        puzzle: crate::full_line::View,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Accepted {
    pub rating: String,
    pub repeat_after: Option<u8>,
    pub answers: Vec<String>,
    pub played_in_game: Option<String>,
}

// The accepted reply keeps the predecessor wire encoding.
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
    pub action: AnswerAction,
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
