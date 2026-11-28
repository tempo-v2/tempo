//! Forward optional one-position analysis to the local Stockfish companion.
use crate::{
    ApiError, App, ViewResult, chess, repertoire_similarity::PositionIr,
    tactic_worker_protocol as wire,
};
use anyhow::{Result, ensure};
use axum::{Json, extract::State, response::IntoResponse};
use pgn_reader::shakmaty::{Chess, Color, Position};
use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{io::AsyncWriteExt, net::UnixStream, sync::Semaphore, time::timeout};

// Scores on the private engine interface are relative to White. Consumers
// compare them from the side to move, keeping mate distance separate from CP.
impl wire::Score {
    pub(crate) fn order(self, side: wire::Side) -> (i8, i64) {
        match self {
            Self::Centipawns { white } => (
                0,
                i64::from(white) * if side == wire::Side::White { 1 } else { -1 },
            ),
            Self::Mate { winner, moves } if winner == side => (1, -i64::from(moves)),
            Self::Mate { moves, .. } => (-1, i64::from(moves)),
        }
    }
}

/// Require one valid score for every legal root before comparing evaluations.
pub fn rank_roots(board: &Chess, mut lines: Vec<wire::Root>) -> Result<Vec<wire::Root>> {
    let mut legal: std::collections::HashSet<_> = chess::legal(board).into_iter().collect();
    ensure!(
        !legal.is_empty()
            && lines.len() == legal.len()
            && lines.iter().all(|root| legal.remove(&root.first_move)
                && !matches!(root.score, wire::Score::Mate { moves: 0, .. })),
        "Incomplete position evaluation"
    );
    let side = if board.turn() == Color::White {
        wire::Side::White
    } else {
        wire::Side::Black
    };
    lines.sort_by(|a, b| {
        b.score
            .order(side)
            .cmp(&a.score.order(side))
            .then(a.first_move.cmp(&b.first_move))
    });
    Ok(lines)
}

static SLOT: Semaphore = Semaphore::const_new(1);

pub struct Endpoint {
    path: PathBuf,
    owner: u32,
}

impl Endpoint {
    pub fn new(database: &Path, owner: u32) -> Self {
        Self {
            path: wire::engine_path(database),
            owner,
        }
    }

    async fn connect(&self) -> io::Result<UnixStream> {
        let stream = UnixStream::connect(&self.path).await?;
        if stream.peer_cred()?.uid() != self.owner {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Unexpected engine peer",
            ));
        }
        Ok(stream)
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowseRequest {
    fen: String,
}

pub async fn browse(State(app): State<App>, Json(request): Json<BrowseRequest>) -> ViewResult {
    PositionIr::parse(&request.fen).map_err(|error| ApiError(error.into()))?;
    Ok(Json(
        evaluate(
            app.opening_engine.as_ref(),
            wire::EngineRequest {
                fen: request.fen,
                mode: wire::EngineMode::TopFive,
            },
        )
        .await,
    )
    .into_response())
}

pub async fn evaluate(
    endpoint: Option<&Endpoint>,
    request: wire::EngineRequest,
) -> wire::EngineReply {
    let Some(endpoint) = endpoint else {
        return wire::EngineReply::Unavailable;
    };
    let Ok(_permit) = SLOT.try_acquire() else {
        return wire::EngineReply::Busy;
    };
    let result = timeout(Duration::from_secs(5), async {
        let mut stream = endpoint.connect().await?;
        let mut message = serde_json::to_vec(&request)?;
        message.push(b'\n');
        stream.write_all(&message).await?;
        wire::read::<wire::EngineReply>(&mut stream).await
    })
    .await;
    match result {
        Ok(Ok(reply)) => reply,
        _ => wire::EngineReply::Unavailable,
    }
}
