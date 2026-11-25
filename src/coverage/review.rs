//! Optional move evidence for one addressed Coverage gap.
use crate::{
    ApiError, App, chess, opening_engine, optional_read,
    repertoire_similarity::{self as similarity, Candidate, Example, PositionIr},
    tactic_worker_protocol::{EngineMode, EngineReply, EngineRequest, Root, Score, Side},
};
use anyhow::{Result, ensure};
use axum::{Json, extract::State};
use pgn_reader::shakmaty::{Chess, Color, Position, san::SanPlus};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tokio::sync::Semaphore;

static SLOT: Semaphore = Semaphore::const_new(1);
const DISPLAY_COUNT: usize = 5;
const MAX_LOSS_CP: i64 = 30;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Address {
    report: String,
    gap: usize,
}

struct Gap {
    fen: String,
    repertoire: String,
    color: String,
    revision: i64,
}

enum Target {
    Fresh(Gap),
    Resolved(Review),
}

#[derive(Serialize)]
pub struct Reply {
    #[serde(flatten)]
    address: Address,
    #[serde(flatten)]
    review: Review,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Review {
    Covered {
        san: String,
    },
    Conflict,
    Stale,
    Terminal,
    Busy,
    Unavailable,
    Ranked {
        best: EvaluatedMove,
        winning_mate: bool,
        similarity_available: bool,
        choices: Vec<Choice>,
    },
}

#[derive(Serialize)]
struct EvaluatedMove {
    first_move: String,
    san: String,
    score: Score,
}

#[derive(Serialize)]
struct Choice {
    #[serde(flatten)]
    movement: EvaluatedMove,
    distance: Option<u32>,
    example: Option<Example>,
}

pub async fn review(
    State(app): State<App>,
    Json(address): Json<Address>,
) -> Result<Json<Reply>, ApiError> {
    let result = match SLOT.try_acquire() {
        Ok(_permit) => inspect(&app, &address).await.map_err(ApiError)?,
        Err(_) => Review::Busy,
    };
    Ok(Json(Reply {
        address,
        review: result,
    }))
}

async fn inspect(app: &App, address: &Address) -> Result<Review> {
    let report = address.report.clone();
    let index = address.gap;
    let target = optional_read::read_value(
        optional_read::Reader::RepertoireSimilarity,
        app.database.clone(),
        move |db, _| {
            let tx = db.transaction()?;
            let (scan, revision, gap) = super::selected(&tx, &report, index)?;
            let board = chess::position(&gap.fen)?;
            ensure!(
                chess::color_name(board.turn()) == scan.color,
                "Unexpected gap side to move"
            );
            if let Some(reply) = crate::catalog::current_opening_reply(
                &tx,
                &scan.repertoire,
                &chess::position_id(&board),
            )? {
                if reply.quarantined {
                    return Ok(Target::Resolved(Review::Conflict));
                }
                let [movement]: [String; 1] = serde_json::from_str(&reply.answers)?;
                return Ok(Target::Resolved(Review::Covered {
                    san: san(&board, &movement)?,
                }));
            }
            if super::revision(&tx, &scan.repertoire)? != revision {
                return Ok(Target::Resolved(Review::Stale));
            }
            Ok(Target::Fresh(Gap {
                fen: gap.fen,
                repertoire: scan.repertoire,
                color: scan.color,
                revision,
            }))
        },
    )
    .await?;
    let gap = match target {
        Target::Resolved(result) => return Ok(result),
        Target::Fresh(gap) => gap,
    };

    let board = chess::position(&gap.fen)?;
    let position = PositionIr::parse(&gap.fen)?;
    let repertoire = gap.repertoire.clone();
    let candidates = optional_read::read_value(
        optional_read::Reader::RepertoireSimilarity,
        app.database.clone(),
        move |db, stopped| {
            let tx = db.transaction()?;
            let input = similarity::snapshot(&tx, &repertoire, &gap.color)?;
            drop(tx);
            Ok(similarity::all_candidates(position, input, stopped)?)
        },
    );
    let engine = opening_engine::evaluate(
        app,
        EngineRequest {
            fen: gap.fen,
            mode: EngineMode::ScoredRoots,
        },
    );
    let (candidates, evidence) = tokio::join!(candidates, engine);
    let result = match evidence {
        EngineReply::Busy => return Ok(Review::Busy),
        EngineReply::Terminal if board.legal_moves().is_empty() => return Ok(Review::Terminal),
        EngineReply::Evaluated {
            engine,
            depth: 10,
            lines,
        } if !engine.is_empty() && engine.len() <= 128 => {
            rank(&board, candidates.ok(), lines).unwrap_or(Review::Unavailable)
        }
        _ => return Ok(Review::Unavailable),
    };
    let fresh = optional_read::read_value(
        optional_read::Reader::RepertoireSimilarity,
        app.database.clone(),
        move |db, _| Ok(super::revision(db, &gap.repertoire)? == gap.revision),
    )
    .await?;
    Ok(if fresh { result } else { Review::Stale })
}

fn san(board: &Chess, movement: &str) -> Result<String> {
    Ok(SanPlus::from_move(board.clone(), chess::parse_move(board, movement)?).to_string())
}

fn rank(board: &Chess, candidates: Option<Vec<Candidate>>, lines: Vec<Root>) -> Result<Review> {
    let mut legal: HashSet<_> = chess::legal(board).into_iter().collect();
    ensure!(
        !legal.is_empty()
            && lines.len() == legal.len()
            && lines.iter().all(|root| legal.remove(&root.first_move)
                && !matches!(root.score, Score::Mate { moves: 0, .. })),
        "Incomplete gap evaluation"
    );
    let side = if board.turn() == Color::White {
        Side::White
    } else {
        Side::Black
    };
    let mut lines = lines;
    lines.sort_by(|a, b| {
        b.score
            .order(side)
            .cmp(&a.score.order(side))
            .then(a.first_move.cmp(&b.first_move))
    });
    let best = &lines[0];
    let best_view = EvaluatedMove {
        san: san(board, &best.first_move)?,
        first_move: best.first_move.clone(),
        score: best.score,
    };
    let winning_mate = matches!(best.score, Score::Mate { winner, .. } if winner == side);
    let similarity_available = candidates.is_some();
    if !similarity_available && matches!(best.score, Score::Centipawns { .. }) {
        return Ok(Review::Unavailable);
    }
    let mut familiar: HashMap<_, _> = candidates
        .unwrap_or_default()
        .into_iter()
        .map(|candidate| (candidate.first_move.clone(), candidate))
        .collect();
    let mut choices = Vec::new();
    for root in lines {
        let matched = familiar.remove(&root.first_move);
        let eligible = match (best_view.score, root.score) {
            (Score::Mate { .. }, Score::Mate { winner, .. }) => winning_mate && winner == side,
            (Score::Centipawns { .. }, Score::Centipawns { .. }) => {
                matched.is_some()
                    && best_view.score.order(side).1 - root.score.order(side).1 <= MAX_LOSS_CP
            }
            _ => false,
        };
        if eligible {
            let notation = match &matched {
                Some(saved) => saved.san.clone(),
                None => san(board, &root.first_move)?,
            };
            choices.push(Choice {
                movement: EvaluatedMove {
                    first_move: root.first_move,
                    san: notation,
                    score: root.score,
                },
                distance: matched.as_ref().map(|saved| saved.distance),
                example: matched.map(|saved| saved.example),
            });
        }
    }
    choices.sort_by(|a, b| {
        let distance = || match (a.distance, b.distance) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        };
        let engine = || {
            b.movement
                .score
                .order(side)
                .cmp(&a.movement.score.order(side))
        };
        (if winning_mate {
            engine().then_with(distance)
        } else {
            distance().then_with(engine)
        })
        .then(a.movement.first_move.cmp(&b.movement.first_move))
    });
    if !winning_mate {
        choices.truncate(DISPLAY_COUNT);
    }
    Ok(Review::Ranked {
        best: best_view,
        winning_mate,
        similarity_available,
        choices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example(board: &Chess, first_move: &str, distance: u32) -> Candidate {
        Candidate {
            first_move: first_move.into(),
            san: san(board, first_move).unwrap(),
            distance,
            example: Example {
                position_id: chess::position_id(board),
                fen: chess::fen(board),
                answer_version: "fixture".into(),
            },
        }
    }

    fn evaluate(board: &Chess, overrides: &[(&str, Score)], default: Score) -> Vec<Root> {
        chess::legal(board)
            .into_iter()
            .map(|first_move| Root {
                score: overrides
                    .iter()
                    .find_map(|(uci, score)| (*uci == first_move).then_some(*score))
                    .unwrap_or(default),
                first_move,
            })
            .collect()
    }

    #[test]
    fn cutoff_is_inclusive_and_precedes_the_display_limit_for_either_side() {
        for color in ["w", "b"] {
            let board = chess::position(&format!(
                "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR {color} KQkq - 0 1"
            ))
            .unwrap();
            let roots = chess::legal(&board);
            let sign = if color == "w" { 1 } else { -1 };
            let cp = |points: i32| Score::Centipawns {
                white: points * sign,
            };
            let evidence = evaluate(
                &board,
                &[
                    (&roots[9], cp(80)),
                    (&roots[5], cp(50)),
                    (&roots[6], cp(65)),
                    (&roots[7], cp(60)),
                ],
                cp(49),
            );
            let candidates = roots
                .iter()
                .take(8)
                .enumerate()
                .map(|(i, movement)| example(&board, movement, (i as u32).min(6)))
                .collect();
            let Review::Ranked { best, choices, .. } =
                rank(&board, Some(candidates), evidence).unwrap()
            else {
                panic!("Expected ranked replies")
            };
            assert_eq!(best.first_move, roots[9]);
            // Five closer replies at 31 CP lose to qualifying moves. Equal
            // distances for roots 6 and 7 use the engine score, for either side.
            assert_eq!(
                choices
                    .iter()
                    .map(|choice| &choice.movement.first_move)
                    .collect::<Vec<_>>(),
                vec![&roots[5], &roots[6], &roots[7]]
            );
        }
    }

    #[test]
    fn winning_mates_include_unfamiliar_roots_and_prefer_the_shortest() {
        let board = chess::position("").unwrap();
        let mate = |winner, moves| Score::Mate { winner, moves };
        let evidence = evaluate(
            &board,
            &[
                ("e2e4", mate(Side::White, 4)),
                ("d2d4", mate(Side::White, 2)),
                ("g1f3", mate(Side::Black, 3)),
            ],
            Score::Centipawns { white: 10 },
        );
        let Review::Ranked {
            winning_mate,
            choices,
            ..
        } = rank(&board, Some(vec![example(&board, "e2e4", 0)]), evidence).unwrap()
        else {
            panic!("Expected mate replies")
        };
        assert!(winning_mate);
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].movement.first_move, "d2d4");
        assert!(choices[0].example.is_none());
        assert_eq!(choices[1].movement.first_move, "e2e4");
        let losses = evaluate(&board, &[], mate(Side::Black, 3));
        let Review::Ranked {
            winning_mate,
            choices,
            ..
        } = rank(&board, None, losses).unwrap()
        else {
            panic!("Expected best defense")
        };
        assert!(!winning_mate && choices.is_empty());
    }

    #[test]
    fn a_partial_root_set_cannot_rank() {
        let board = chess::position("").unwrap();
        let mut evidence = evaluate(&board, &[], Score::Centipawns { white: 0 });
        evidence.pop();
        assert!(rank(&board, Some(vec![example(&board, "e2e4", 0)]), evidence).is_err());
    }
}
