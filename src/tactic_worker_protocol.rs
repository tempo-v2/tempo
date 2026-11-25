use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io,
    path::{Path, PathBuf},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const LIMIT: usize = 64 * 1024;

pub fn socket_path(database: &Path) -> PathBuf {
    database.with_extension("tq").join("io")
}

pub fn status_path(database: &Path) -> PathBuf {
    socket_path(database).with_file_name("status")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Work {
    pub game: String,
    pub ply: usize,
    pub token: String,
    pub fen: String,
    pub depth: u8,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Next,
    Status,
    Submit { work: Work, evidence: Value },
    Retry { work: Work, error: Failure },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SubmitReply {
    Ok { outcome: Verdict },
    InvalidEvidence,
    StaleWork,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Kept,
    Narrowed,
    Published,
    Equivalent,
    BestOutsideCandidates,
    PlayedBest,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    Busy,
    EngineUnavailable,
    Timeout,
    Cancelled,
    Protocol,
    Incomplete,
    InvalidRequest,
    InvalidEvidence,
}

pub async fn read<T: serde::de::DeserializeOwned>(
    stream: &mut (impl AsyncRead + Unpin),
) -> io::Result<T> {
    let mut bytes = Vec::new();
    stream
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Tactic message exceeds byte limit",
        ));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub async fn send<T: Serialize>(
    stream: &mut (impl AsyncWrite + Unpin),
    message: &T,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(message)?;
    if bytes.len() > LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Tactic message exceeds byte limit",
        ));
    }
    stream.write_all(&bytes).await?;
    stream.shutdown().await
}

pub fn engine_path(database: &Path) -> PathBuf {
    socket_path(database).with_file_name("engine")
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EngineRequest {
    pub fen: String,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EngineReply {
    Ranked { first_moves: Vec<String> },
    Terminal,
    Busy,
    Unavailable,
}
