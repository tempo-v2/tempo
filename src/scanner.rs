use crate::{catalog::ReferenceAnswer, chess};
use anyhow::Result;
use pgn_reader::shakmaty::Position;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct RepertoireMismatch {
    pub game_id: String,
    pub ply: usize,
    pub position_id: String,
    pub repertoire_id: String,
    pub answer_version: String,
    pub played_move: String,
}

#[derive(Debug)]
pub struct MissedCaptureFound {
    pub ply: usize,
    pub position_id: String,
    pub fen: String,
    pub played_move: String,
    pub captures: Vec<String>,
}

pub struct Findings {
    pub mismatches: Vec<RepertoireMismatch>,
    pub captures: Vec<MissedCaptureFound>,
}

/// Compare a game against the active replies supplied by the opening catalog.
pub fn scan(
    game_id: &str,
    route: &chess::Route,
    learner_color: &str,
    mut references: impl FnMut(&str, &str) -> Result<Vec<ReferenceAnswer>>,
) -> Result<Findings> {
    let mut board = chess::position(&route.fen)?;
    let mut mismatches = Vec::new();
    let mut captures = Vec::new();
    for (ply, played) in route.moves.iter().enumerate() {
        let m = chess::parse_move(&board, played)?;
        if chess::color_name(board.turn()) == learner_color {
            let position_id = chess::position_id(&board);
            let reference = references(&position_id, learner_color)?;
            mismatches.extend(
                reference
                    .into_iter()
                    .filter(|item| !item.moves.contains(played))
                    .map(|item| RepertoireMismatch {
                        game_id: game_id.to_owned(),
                        ply,
                        position_id: position_id.clone(),
                        repertoire_id: item.repertoire_id,
                        answer_version: item.answer_version,
                        played_move: played.clone(),
                    }),
            );
            let options = chess::missed_captures(&board, m);
            if !options.is_empty() {
                captures.push(MissedCaptureFound {
                    ply,
                    position_id,
                    fen: chess::fen(&board),
                    played_move: played.clone(),
                    captures: options,
                });
            }
        }
        board.play_unchecked(m);
    }

    Ok(Findings {
        mismatches,
        captures,
    })
}
