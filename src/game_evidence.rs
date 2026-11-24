//! Answer-safe game views and immutable legacy command replay.

use crate::adapters::public_games::Provider;
use crate::{AppError, Tempo};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempo_learning::GameReviewEffect;
use tempo_store::GameSummary;
use tempo_types::DatasetId;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GameImport {
    pub command_id: String,
    pub dataset_id: DatasetId,
    pub learner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repertoire_id: Option<String>,
    pub pgn: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GameRefresh {
    command_id: String,
    dataset_id: DatasetId,
    source: Provider,
    account: String,
    from_ms: u64,
    to_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repertoire_id: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReprocessGames {
    pub command_id: String,
    pub dataset_id: DatasetId,
    pub learner: String,
    pub from_ms: u64,
    pub to_ms: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChooseGameCorrection {
    pub command_id: String,
    pub dataset_id: DatasetId,
    pub learner: String,
    pub source: String,
    pub game_id: String,
    pub comparison_id: String,
    pub ply: usize,
    pub target_id: tempo_types::TargetId,
}

#[derive(Default, Serialize, Deserialize)]
pub struct GameReport {
    pub fetched: usize,
    pub new_games: usize,
    pub duplicates: usize,
    pub opposite_color_skipped: usize,
    pub matches: usize,
    pub deviations: usize,
    pub errors: Vec<String>,
    pub results: Vec<GameResult>,
}

#[derive(Serialize, Deserialize)]
pub struct GameResult {
    /// Checkpoint 12 receipts preserve their original shape on replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison_id: Option<String>,
    #[serde(flatten)]
    pub summary: GameSummary,
    pub effect: GameReviewEffect,
}

pub(crate) fn fingerprint<T: Serialize>(kind: &str, request: &T) -> Result<String, AppError> {
    let payload = serde_json::to_vec(&(kind, request))
        .map_err(|error| AppError::new("invalid_game", error.to_string()))?;
    Ok(blake3::hash(&payload).to_hex().to_string())
}

pub(crate) fn check_id(id: &str) -> Result<(), AppError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(AppError::new(
            "invalid_game",
            "An immutable game command ID is required.",
        ));
    }
    Ok(())
}

impl ReprocessGames {
    /// Resolve the previous synchronous result by its unchanged command fingerprint.
    pub(crate) fn replay(self, app: &Tempo) -> Result<(u16, Value), AppError> {
        check_id(&self.command_id)?;
        if app.game_dataset()? != self.dataset_id {
            return Err(AppError::new(
                "stale_dataset",
                "The active dataset changed.",
            ));
        }
        let key = fingerprint("reprocess", &self)?;
        if let Some(report) = app.replay_game(&self.dataset_id, &self.command_id, &key)? {
            return Ok((200, json!(report)));
        }
        Ok((
            410,
            json!({
                "status": "error", "code": "retired_feature",
                "message": "This reprocess request was not accepted. Reprocessing saved games has been retired.",
                "retryable": false, "dataset_id": self.dataset_id, "command_id": self.command_id,
            }),
        ))
    }
}

impl ChooseGameCorrection {
    pub(crate) fn replay(self, app: &Tempo) -> Result<(u16, Value), AppError> {
        let dataset_id = self.dataset_id.clone();
        let command_id = self.command_id.clone();
        match app.choose_game_correction(self) {
            Ok(report) => Ok((200, json!(report))),
            Err(error) if error.code == "retired_feature" => Ok((410, json!({
                "status": "error", "code": error.code, "message": error.message,
                "retryable": false, "dataset_id": dataset_id, "command_id": command_id,
            }))),
            Err(error) => Err(error),
        }
    }
}

impl GameRefresh {
    /// Interpret the exact legacy input solely to recover its accepted receipt.
    pub(crate) fn replay(self, app: &Tempo) -> Result<(u16, Value), AppError> {
        check_id(&self.command_id)?;
        if app.game_dataset()? != self.dataset_id {
            return Err(AppError::new(
                "stale_dataset",
                "The active dataset changed.",
            ));
        }
        let key = fingerprint("refresh", &self)?;
        if let Some(report) = app.replay_game(&self.dataset_id, &self.command_id, &key)? {
            return Ok((
                202,
                json!({
                    "status": "complete", "dataset_id": self.dataset_id,
                    "command_id": self.command_id, "report": report,
                }),
            ));
        }
        Ok((
            410,
            json!({
                "status": "error", "code": "retired_feature",
                "message": "This refresh was not accepted. Remote game refresh has been retired.",
                "retryable": false, "dataset_id": self.dataset_id, "command_id": self.command_id,
            }),
        ))
    }
}

#[cfg(test)]
mod tests {
    mod refresh_replay;
}
