//! Full-line Study values and progression over immutable issued moves.
use crate::{
    chess::{self, Route},
    learning::Rating,
    study::{AnswerAction, Conflict, RepeatDisposition},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// A source's opaque identity and immutable, Chess-validated solver route.
pub struct PublishedExercise {
    key: String,
    version: String,
    route: Route,
}

impl PublishedExercise {
    pub fn new(key: String, version: String, route: Route) -> Result<Self> {
        ensure!(
            !key.is_empty() && !version.is_empty(),
            "Full-line identity is missing"
        );
        let route = chess::validate_solver_route(route)?;
        Ok(Self {
            key,
            version,
            route,
        })
    }

    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn version(&self) -> &str {
        &self.version
    }
    pub fn route(&self) -> &Route {
        &self.route
    }
}

pub struct InitialCard {
    pub id: String,
    pub title: String,
}

#[derive(Deserialize, Serialize)]
pub struct Move {
    pub dataset: String,
    pub play: i64,
    pub step: i64,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub played: Option<String>,
}
#[derive(Serialize, Deserialize)]
pub struct Outcome {
    pub rating: Rating,
    pub repeat: RepeatDisposition,
    pub due_day: Option<String>,
}
#[derive(Serialize, Deserialize)]
pub struct View {
    pub dataset: String,
    pub play: i64,
    pub puzzle: String,
    pub step: i64,
    pub mode: String,
    pub fen: String,
    pub color: String,
    pub legal: Vec<String>,
    pub last_move: Option<String>,
    pub result: Option<Outcome>,
    pub solution: Option<Route>,
}

#[derive(Serialize)]
pub struct MoveReply {
    #[serde(flatten)]
    pub view: View,
    pub before_fen: String,
}

#[derive(Clone, Copy)]
pub enum Surface {
    Tactics,
    Study,
}
pub struct Step {
    pub ply: usize,
    pub rating: Option<Rating>,
    pub final_move: Option<String>,
}

pub fn advance(route: &Route, ply: usize, action: &AnswerAction) -> Result<Step> {
    use chess::PuzzleMove;
    let (ply, rating, final_move) = match chess::score_puzzle_move(route, ply, action.played())? {
        PuzzleMove::Illegal => return Err(Conflict::IllegalMove.into()),
        PuzzleMove::Again => (ply, Some(Rating::Again), None),
        PuzzleMove::Continue { ply } => (ply, None, None),
        PuzzleMove::Solved { ply, alternate } => (ply, Some(Rating::Good), alternate),
    };
    Ok(Step {
        ply,
        rating,
        final_move,
    })
}
