//! Openings' authored prerequisite and forecast projections. Both use Chess's
//! canonical position identity and legal replay.

use crate::{chess, daily_plan::Prerequisites};
use anyhow::{Result, ensure};
use pgn_reader::shakmaty::Position;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

pub struct SavedLine {
    pub repertoire: String,
    pub color: String,
    pub route: chess::Route,
    pub start_at: usize,
}

/// The exact active reply for a canonical position in one repertoire.
pub struct Reply {
    pub card: String,
    pub repertoire: String,
    pub position_id: String,
    pub answer: String,
}

pub struct OpeningProjection {
    pub prerequisites: HashMap<String, Prerequisites>,
    pub routes: Vec<ForecastRoute>,
}

/// The active reply applies wherever its position is reached in this repertoire,
/// including paths that began in a different authored line.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForecastReply {
    pub repertoire: String,
    pub color: String,
    pub fen: String,
    pub movement: String,
    pub card: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForecastRoute {
    pub repertoire: String,
    pub color: String,
    pub fen: String,
    pub moves: Vec<String>,
    pub decisions: Vec<ForecastDecision>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForecastDecision {
    /// Zero-based ply of the position before the authored learner move.
    pub ply: usize,
    pub card: String,
}

#[derive(Clone, Copy)]
pub enum Projection {
    Prerequisites,
    Forecast,
    #[cfg(test)]
    Both,
}

/// Compile prerequisite alternatives and forecast histories from the same
/// coherent authored snapshot. A card with no proved route remains unresolved.
pub fn compile_openings(
    lines: &[SavedLine],
    replies: &[Reply],
    projection: Projection,
    stop: impl Fn() -> bool,
) -> Result<OpeningProjection> {
    let include_forecast = !matches!(projection, Projection::Prerequisites);
    let include_prerequisites = !matches!(projection, Projection::Forecast);
    let mut current = HashMap::new();
    let mut alternatives = HashMap::new();
    let mut cards = HashSet::new();
    for reply in replies {
        ensure!(!stop(), "Opening projection expired");
        ensure!(
            current
                .insert(
                    (reply.repertoire.as_str(), reply.position_id.as_str()),
                    reply
                )
                .is_none(),
            "More than one current reply for a position in the same repertoire"
        );
        ensure!(
            cards.insert(reply.card.as_str()),
            "Duplicate current opening card identity"
        );
        if include_prerequisites {
            alternatives.insert(reply.card.clone(), BTreeSet::<Vec<String>>::new());
        }
    }

    let mut routes = Vec::new();
    for line in lines {
        ensure!(!stop(), "Opening projection expired");
        ensure!(
            matches!(line.color.as_str(), "white" | "black")
                && line.start_at < line.route.moves.len()
                && line.route.moves.len() <= chess::MAX_PLIES,
            "Invalid saved opening route scope"
        );
        let mut board = chess::position(&line.route.fen)?;
        let mut route = include_forecast.then(|| ForecastRoute {
            repertoire: line.repertoire.clone(),
            color: line.color.clone(),
            fen: chess::fen(&board),
            moves: Vec::with_capacity(line.route.moves.len()),
            decisions: Vec::new(),
        });
        let mut parents = BTreeSet::<&str>::new();
        let mut recorded = HashSet::new();
        let mut complete = true;
        for (ply, input) in line.route.moves.iter().enumerate() {
            let movement = chess::parse_move(&board, input)?;
            let canonical = chess::uci(movement);
            if chess::color_name(board.turn()) == line.color {
                let position = chess::position_id(&board);
                let reply = current.get(&(line.repertoire.as_str(), position.as_str()));
                if let Some(reply) = reply.filter(|reply| reply.answer == canonical) {
                    if let Some(route) = &mut route {
                        route.decisions.push(ForecastDecision {
                            ply,
                            card: reply.card.clone(),
                        });
                    }
                    if include_prerequisites {
                        if complete && ply >= line.start_at && recorded.insert(reply.card.as_str())
                        {
                            alternatives.get_mut(&reply.card).unwrap().insert(
                                parents
                                    .iter()
                                    .copied()
                                    .filter(|parent| *parent != reply.card)
                                    .map(str::to_owned)
                                    .collect(),
                            );
                        }
                        parents.insert(reply.card.as_str());
                    }
                } else if ply >= line.start_at || reply.is_some() {
                    // A missing owned reply or incompatible saved learner move
                    // prevents this prefix from proving a prerequisite route.
                    // Retain the legal history so the forecaster can account for
                    // unsupported paths and context ambiguity independently.
                    complete = false;
                }
            }
            board.play_unchecked(movement);
            if let Some(route) = &mut route {
                route.moves.push(canonical);
            }
        }
        if let Some(route) = route {
            routes.push(route);
        }
    }

    ensure!(!stop(), "Opening projection expired");
    let prerequisites = alternatives
        .into_iter()
        .map(|(card, routes)| {
            let status = if routes.is_empty() {
                Prerequisites::Unknown
            } else if routes.first().is_some_and(Vec::is_empty) {
                Prerequisites::NoneRequired
            } else {
                Prerequisites::Routes(routes.into_iter().collect())
            };
            (card, status)
        })
        .collect();
    Ok(OpeningProjection {
        prerequisites,
        routes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(repertoire: &str, moves: &[&str], start_at: usize) -> SavedLine {
        SavedLine {
            repertoire: repertoire.to_owned(),
            color: "white".to_owned(),
            route: chess::Route {
                fen: String::new(),
                moves: moves
                    .iter()
                    .map(|movement| (*movement).to_owned())
                    .collect(),
            },
            start_at,
        }
    }

    fn reply(line: &SavedLine, ply: usize, card: &str) -> Reply {
        Reply {
            card: card.to_owned(),
            repertoire: line.repertoire.clone(),
            position_id: chess::position_id(&chess::board_at(&line.route, ply).unwrap()),
            answer: line.route.moves[ply].clone(),
        }
    }

    #[test]
    fn transposed_replies_keep_alternative_parent_sets_and_scope() -> Result<()> {
        let first = line("white", &["d2d4", "d7d5", "g1f3", "g8f6", "c2c4"], 0);
        let second = line("white", &["d2d4", "g8f6", "g1f3", "d7d5", "c2c4"], 2);
        let elsewhere = line("other", &["d2d4", "g8f6", "g1f3"], 2);
        let replies = [
            reply(&first, 0, "root"),
            reply(&first, 2, "first"),
            reply(&second, 2, "second"),
            reply(&first, 4, "target"),
            reply(&elsewhere, 2, "other"),
        ];
        assert_eq!(
            replies[3].position_id,
            reply(&second, 4, "same").position_id
        );
        let compiled = compile_openings(
            &[first, second, elsewhere],
            &replies,
            Projection::Both,
            || false,
        )?;
        assert_eq!(compiled.prerequisites["root"], Prerequisites::NoneRequired);
        assert_eq!(compiled.prerequisites["other"], Prerequisites::NoneRequired);
        assert_eq!(
            compiled.prerequisites["target"],
            Prerequisites::Routes(vec![
                vec!["first".to_owned(), "root".to_owned()],
                vec!["root".to_owned(), "second".to_owned()],
            ])
        );
        assert_eq!(compiled.routes[1].decisions[0].ply, 0);
        assert_eq!(compiled.routes[1].decisions[0].card, "root");
        assert_eq!(compiled.routes[2].decisions[0].ply, 2);
        Ok(())
    }

    #[test]
    fn absent_active_parent_cannot_turn_an_owned_prefix_into_a_root() -> Result<()> {
        let first = line("white", &["d2d4", "d7d5", "g1f3"], 0);
        let replies = [reply(&first, 2, "child")];
        let compiled = compile_openings(&[first], &replies, Projection::Both, || false)?;
        assert_eq!(compiled.prerequisites["child"], Prerequisites::Unknown);
        assert_eq!(compiled.routes[0].decisions[0].card, "child");
        Ok(())
    }

    #[test]
    fn a_replaced_reply_cannot_satisfy_a_route_using_its_retired_move() -> Result<()> {
        let current = line("white", &["e2e4"], 0);
        let continuation = line("white", &["d2d4", "d7d5", "g1f3"], 2);
        let replies = [
            reply(&current, 0, "new-reply"),
            reply(&continuation, 2, "descendant"),
        ];
        let compiled =
            compile_openings(&[current, continuation], &replies, Projection::Both, || {
                false
            })?;
        assert_eq!(compiled.prerequisites["descendant"], Prerequisites::Unknown);
        assert_eq!(compiled.routes[1].decisions[0].ply, 2);
        Ok(())
    }

    #[test]
    fn repetition_never_creates_a_self_prerequisite() -> Result<()> {
        let first = line("white", &["g1f3", "g8f6", "f3g1", "f6g8", "g1f3"], 0);
        let replies = [reply(&first, 0, "root"), reply(&first, 2, "middle")];
        let compiled = compile_openings(&[first], &replies, Projection::Both, || false)?;
        assert_eq!(compiled.prerequisites["root"], Prerequisites::NoneRequired);
        assert_eq!(
            compiled.prerequisites["middle"],
            Prerequisites::Routes(vec![vec!["root".to_owned()]])
        );
        assert_eq!(compiled.routes[0].decisions[2].card, "root");
        Ok(())
    }
}
