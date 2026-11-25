use crate::{chess, game_ir::Mainline};
use anyhow::Result;
use pgn_reader::shakmaty::{Chess, Color, Move, Position};
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

// Repertoire data is supplied by Openings. No detector can fetch its own inputs.
pub struct ReferenceAnswer {
    pub repertoire_id: String,
    pub answer_version: String,
    pub moves: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Detector {
    Repertoire,
    Capture,
}

/// The selected detector owns its algorithm and its supported versions.
pub enum Program {
    Repertoire(fn(&Decision, ReferenceAnswer) -> Option<Observation>),
    Capture(fn(&Decision) -> Option<Observation>),
}

impl Detector {
    pub fn current_version(self) -> i64 {
        match self {
            Self::Repertoire => 1,
            Self::Capture => 1,
        }
    }

    pub fn program(self, version: i64) -> Result<Program> {
        match (self, version) {
            (Self::Repertoire, 1) => Ok(Program::Repertoire(compare)),
            (Self::Capture, 1) => Ok(Program::Capture(capture)),
            _ => anyhow::bail!("unsupported detector version"),
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

pub fn compare(decision: &Decision, reference: ReferenceAnswer) -> Option<Observation> {
    (!reference.moves.contains(&decision.uci)).then(|| Observation::Repertoire {
        ply: decision.ply,
        position: decision.position.clone(),
        repertoire: reference.repertoire_id,
        answer_version: reference.answer_version,
        played: decision.uci.clone(),
    })
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
