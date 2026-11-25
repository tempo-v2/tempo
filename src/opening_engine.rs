//! Forward optional one-position analysis to the local Stockfish companion.
use crate::{
    ApiError, App, ViewResult, repertoire_similarity::PositionIr, tactic_worker_protocol as wire,
};
use axum::{Json, extract::State, response::IntoResponse};
use std::time::Duration;
use tokio::{io::AsyncWriteExt, net::UnixStream, sync::Semaphore, time::timeout};

static SLOT: Semaphore = Semaphore::const_new(1);

pub async fn browse(
    State(app): State<App>,
    Json(request): Json<wire::EngineRequest>,
) -> ViewResult {
    PositionIr::parse(&request.fen).map_err(|error| ApiError(error.into()))?;
    let unavailable = || Json(wire::EngineReply::Unavailable).into_response();
    let Ok(_permit) = SLOT.try_acquire() else {
        return Ok(Json(wire::EngineReply::Busy).into_response());
    };
    let result = timeout(Duration::from_secs(5), async {
        let mut stream = UnixStream::connect(wire::engine_path(&app.database)).await?;
        let mut message = serde_json::to_vec(&request)?;
        message.push(b'\n');
        stream.write_all(&message).await?;
        wire::read::<wire::EngineReply>(&mut stream).await
    })
    .await;
    Ok(match result {
        Ok(Ok(reply)) => Json(reply).into_response(),
        _ => unavailable(),
    })
}
