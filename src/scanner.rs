use crate::{chess, game_ir::Mainline};
use anyhow::Result;
use pgn_reader::shakmaty::{Chess, Color, Move, Position, san::SanPlus};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum Observation {
    Repertoire {
        ply: usize,
        position: String,
        repertoire: String,
        answer_version: String,
        played: String,
    },
    Capture {
        ply: usize,
        position: String,
        fen: String,
        played: String,
        captures: Vec<String>,
    },
}

/// The authored reply supplied by Openings for one learner position.
pub enum AuthoredReply {
    Missing,
    Conflict,
    Unusable,
    Expected { uci: String, version: String },
}

pub const REPERTOIRE_VERSION: i64 = 2;

#[derive(Serialize, Deserialize)]
pub struct RepertoireDecision {
    pub ply: usize,
    pub position: String,
    pub move_number: u32,
    pub played: MoveLabel,
    #[serde(flatten)]
    pub outcome: RepertoireOutcome,
}

#[derive(Serialize, Deserialize)]
pub struct MoveLabel {
    pub uci: String,
    pub san: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RepertoireOutcome {
    Followed {
        expected: MoveLabel,
        answer_version: String,
    },
    Differed {
        expected: MoveLabel,
        answer_version: String,
    },
    Unassessed {
        reason: Unassessed,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unassessed {
    NoReply,
    Conflict,
    Unusable,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Detector {
    Repertoire,
    Capture,
}

pub type CaptureProgram = fn(&Decision) -> Option<Observation>;

pub fn capture_program(version: i64) -> Result<CaptureProgram> {
    match version {
        1 => Ok(capture),
        _ => anyhow::bail!("unsupported capture detector version"),
    }
}

impl Detector {
    pub fn current_version(self) -> i64 {
        match self {
            Self::Repertoire => REPERTOIRE_VERSION,
            Self::Capture => 1,
        }
    }

    pub fn job(self, archive: bool) -> &'static str {
        match (self, archive) {
            (Self::Repertoire, true) => "compare_all",
            (Self::Capture, true) => "capture_all",
            (Self::Repertoire, false) => "compare_new",
            (Self::Capture, false) => "capture_new",
        }
    }

    pub fn from_job(operation: &str) -> Option<(Self, bool)> {
        match operation {
            "compare_all" => Some((Self::Repertoire, true)),
            "capture_all" => Some((Self::Capture, true)),
            "compare_new" => Some((Self::Repertoire, false)),
            "capture_new" => Some((Self::Capture, false)),
            _ => None,
        }
    }

    pub fn accepts(self, observation: &Observation) -> bool {
        matches!(
            (self, observation),
            (Self::Repertoire, Observation::Repertoire { .. })
                | (Self::Capture, Observation::Capture { .. })
        )
    }
}

pub struct Decision {
    pub ply: usize,
    pub position: String,
    board: Chess,
    played: Move,
    uci: String,
}

pub fn decisions(game: &Mainline, learner: Color) -> Result<Vec<Decision>> {
    let mut board = chess::position(game.start())?;
    let mut decisions = Vec::new();
    for (ply, uci) in game.moves().enumerate() {
        let played = chess::parse_move(&board, &uci)?;
        if board.turn() == learner {
            decisions.push(Decision {
                ply,
                position: chess::position_id(&board),
                board: board.clone(),
                played,
                uci,
            });
        }
        board.play_unchecked(played);
    }
    Ok(decisions)
}

pub fn compare(decision: &Decision, reply: AuthoredReply) -> RepertoireDecision {
    let label = |uci, movement| MoveLabel {
        uci,
        san: SanPlus::from_move(decision.board.clone(), movement).to_string(),
    };
    let outcome = match reply {
        AuthoredReply::Missing => RepertoireOutcome::Unassessed {
            reason: Unassessed::NoReply,
        },
        AuthoredReply::Conflict => RepertoireOutcome::Unassessed {
            reason: Unassessed::Conflict,
        },
        AuthoredReply::Unusable => RepertoireOutcome::Unassessed {
            reason: Unassessed::Unusable,
        },
        AuthoredReply::Expected { uci, version } => {
            match chess::parse_move(&decision.board, &uci) {
                Ok(movement) if chess::uci(movement) == uci => {
                    let expected = label(uci, movement);
                    if expected.uci == decision.uci {
                        RepertoireOutcome::Followed {
                            expected,
                            answer_version: version,
                        }
                    } else {
                        RepertoireOutcome::Differed {
                            expected,
                            answer_version: version,
                        }
                    }
                }
                _ => RepertoireOutcome::Unassessed {
                    reason: Unassessed::Unusable,
                },
            }
        }
    };
    RepertoireDecision {
        ply: decision.ply,
        position: decision.position.clone(),
        move_number: decision.board.fullmoves().get(),
        played: label(decision.uci.clone(), decision.played),
        outcome,
    }
}

pub fn capture(decision: &Decision) -> Option<Observation> {
    let captures = chess::missed_captures(&decision.board, decision.played);
    (!captures.is_empty()).then(|| Observation::Capture {
        ply: decision.ply,
        position: decision.position.clone(),
        fen: chess::fen(&decision.board),
        played: decision.uci.clone(),
        captures,
    })
}
