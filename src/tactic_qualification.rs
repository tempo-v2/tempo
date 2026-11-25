//! Publication policy for generated single-solution tactics, independent of motif.
use crate::chess;
use anyhow::{Result, ensure};
use pgn_reader::shakmaty::{Color, Position};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, collections::HashSet};

pub const DEPTH: u8 = 10;
const EQUIVALENT_CP: i64 = 30;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    White,
    Black,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Score {
    Centipawns { white: i32 },
    Mate { winner: Side, moves: u32 },
}

impl Score {
    fn order(self, side: Side) -> (i8, i64) {
        match self {
            Self::Centipawns { white } => (
                0,
                i64::from(white) * if side == Side::White { 1 } else { -1 },
            ),
            Self::Mate { winner, moves } if winner == side => (1, -i64::from(moves)),
            Self::Mate { moves, .. } => (-1, i64::from(moves)),
        }
    }

    fn equivalent_to(self, other: Self) -> bool {
        match (self, other) {
            (Self::Centipawns { white: a }, Self::Centipawns { white: b }) => {
                (i64::from(a) - i64::from(b)).abs() <= EQUIVALENT_CP
            }
            (Self::Mate { winner: a, .. }, Self::Mate { winner: b, .. }) => a == b,
            _ => false,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Root {
    pub first_move: String,
    pub score: Score,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Evidence {
    Evaluated {
        engine: String,
        depth: u8,
        lines: Vec<Root>,
    },
    Terminal,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision<'a> {
    Unique(&'a str),
    EquivalentAlternatives,
    BestOutsideCandidates,
}

/// Each proposal must be a legal root. Every legal root competes, whatever its motif.
pub fn qualify<'a>(
    fen: &str,
    candidates: &[String],
    evidence: &'a Evidence,
) -> Result<Decision<'a>> {
    let board = chess::position(fen)?;
    let legal = chess::legal(&board).into_iter().collect::<HashSet<_>>();
    let Evidence::Evaluated {
        engine,
        depth,
        lines,
    } = evidence
    else {
        anyhow::bail!("No complete tactic evaluation")
    };
    ensure!(
        *depth == DEPTH && !engine.is_empty() && engine.len() <= 128,
        "Unexpected tactic evaluation identity"
    );
    ensure!(
        lines
            .iter()
            .all(|root| !matches!(root.score, Score::Mate { moves: 0, .. })),
        "Invalid mate score"
    );
    let scored = lines
        .iter()
        .map(|line| &line.first_move)
        .collect::<HashSet<_>>();
    ensure!(
        scored.len() == lines.len()
            && scored.len() == legal.len()
            && scored.iter().all(|root| legal.contains(*root)),
        "Incomplete tactic root coverage"
    );
    ensure!(
        !candidates.is_empty() && candidates.iter().all(|root| legal.contains(root)),
        "Invalid tactic candidate set"
    );
    let side = match board.turn() {
        Color::White => Side::White,
        Color::Black => Side::Black,
    };
    let best = lines
        .first()
        .ok_or_else(|| anyhow::anyhow!("No legal tactic roots"))?;
    ensure!(
        lines.windows(2).all(
            |pair| pair[0].score.order(side).cmp(&pair[1].score.order(side)) != Ordering::Less
        ),
        "Tactic roots are out of order"
    );
    if lines
        .iter()
        .skip(1)
        .any(|line| best.score.equivalent_to(line.score))
    {
        return Ok(Decision::EquivalentAlternatives);
    }
    if !candidates.contains(&best.first_move) {
        return Ok(Decision::BestOutsideCandidates);
    }
    Ok(Decision::Unique(&best.first_move))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(
        fen: &str,
        best: &str,
        alternate: &str,
        best_score: Score,
        other: Score,
        rest: Score,
    ) -> Result<Evidence> {
        let mut legal = chess::legal(&chess::position(fen)?);
        legal.sort_by_key(|root| match root.as_str() {
            root if root == best => 0,
            root if root == alternate => 1,
            _ => 2,
        });
        Ok(Evidence::Evaluated {
            engine: "fixture".into(),
            depth: DEPTH,
            lines: legal
                .into_iter()
                .map(|first_move| {
                    let score = if first_move == best {
                        best_score
                    } else if first_move == alternate {
                        other
                    } else {
                        rest
                    };
                    Root { first_move, score }
                })
                .collect(),
        })
    }

    #[test]
    fn compares_every_root_at_the_inclusive_boundary_for_either_color() -> Result<()> {
        for (color, best, quiet, sign) in [("w", "e2e4", "c2c3", 1), ("b", "e7e5", "c7c6", -1)] {
            let fen = format!("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR {color} KQkq - 0 1");
            for gap in [29, 30, 31] {
                let evidence = roots(
                    &fen,
                    best,
                    quiet,
                    Score::Centipawns { white: sign * gap },
                    Score::Centipawns { white: 0 },
                    Score::Centipawns { white: -100 * sign },
                )?;
                assert_eq!(
                    qualify(&fen, &[best.into()], &evidence)?,
                    if gap <= 30 {
                        Decision::EquivalentAlternatives
                    } else {
                        Decision::Unique(best)
                    }
                );
                if gap == 31 {
                    assert_eq!(
                        qualify(&fen, &[quiet.into()], &evidence)?,
                        Decision::BestOutsideCandidates
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn slower_forced_mate_is_an_equivalent_solution() -> Result<()> {
        let fen = "6k1/5ppp/8/8/8/8/8/R3R1K1 w - - 0 1";
        let best = "e1e8";
        let alternate = "a1a8";
        let mate = |moves| Score::Mate {
            winner: Side::White,
            moves,
        };
        let evidence = roots(
            fen,
            best,
            alternate,
            mate(1),
            mate(3),
            Score::Centipawns { white: 1200 },
        )?;
        assert_eq!(
            qualify(fen, &[best.into()], &evidence)?,
            Decision::EquivalentAlternatives
        );
        let mut evidence = roots(
            fen,
            best,
            alternate,
            mate(1),
            Score::Centipawns { white: 1200 },
            Score::Centipawns { white: 900 },
        )?;
        assert_eq!(
            qualify(fen, &[best.into()], &evidence)?,
            Decision::Unique(best)
        );
        let Evidence::Evaluated { lines, .. } = &mut evidence else {
            unreachable!()
        };
        lines.pop();
        assert!(qualify(fen, &[best.into()], &evidence).is_err());
        Ok(())
    }
}
