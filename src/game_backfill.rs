//! Addressed readback of accepted historical Games commands.
use crate::adapters::public_games::valid_account;
use crate::game_evidence::check_id;
use crate::{AppError, Tempo};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempo_types::DatasetId;

const DAY: u64 = 86_400_000;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackfillRequest {
    pub command_id: String,
    pub dataset_id: DatasetId,
    pub from_ms: u64,
    pub to_ms: u64,
    pub lichess: Option<String>,
    pub chess_com: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackfillRetry {
    pub dataset_id: DatasetId,
    pub job_id: String,
    pub command_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct BackfillJob {
    pub id: String,
    pub request: BackfillRequest,
    pub resets: BTreeMap<String, u64>,
    pub windows: Vec<Window>,
    retries: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Window {
    source: HistorySource,
    learner: String,
    from_ms: u64,
    to_ms: u64,
    attempt: u32,
    status: WindowStatus,
    received: usize,
    accepted: usize,
    split: bool,
    notes: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum HistorySource {
    Lichess,
    ChessCom,
    Pgn,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WindowStatus {
    Pending,
    Complete,
    Incomplete,
}

impl BackfillJob {
    fn retired_view(&self, command_id: &str) -> Value {
        let pending = self
            .windows
            .iter()
            .filter(|window| window.status == WindowStatus::Pending)
            .count();
        let incomplete = self
            .windows
            .iter()
            .filter(|window| window.status == WindowStatus::Incomplete)
            .count();
        json!({
            "status": if pending + incomplete == 0 { "complete" } else { "incomplete" },
            "execution": "retired", "command_id": command_id,
            "job_id": self.id, "request": self.request, "windows": self.windows,
            "pending_windows": pending, "incomplete_windows": incomplete,
        })
    }
}

impl BackfillRequest {
    pub(crate) fn replay(mut self, app: &Tempo) -> Result<(u16, Value), AppError> {
        check_id(&self.command_id)?;
        require_dataset(app, &self.dataset_id)?;
        for account in [&mut self.lichess, &mut self.chess_com]
            .into_iter()
            .flatten()
        {
            *account = account.trim().to_ascii_lowercase();
            if !valid_account(account) {
                return Err(invalid("Enter a valid provider account."));
            }
        }
        if self.lichess.is_none() && self.chess_com.is_none()
            || self.from_ms % DAY != 0
            || self.to_ms % DAY != 0
            || !matches!(
                self.to_ms.checked_sub(self.from_ms),
                Some(1..=31_622_400_000)
            )
        {
            return Err(invalid(
                "Choose accounts and a completed UTC date range of at most 366 days.",
            ));
        }
        let Some(saved) = app.read_backfill(&self.dataset_id, &self.command_id)? else {
            return Ok(not_accepted(
                &self.dataset_id,
                &self.command_id,
                &self.command_id,
            ));
        };
        if self != saved.request || self.command_id != saved.id {
            return Err(invalid(
                "The backfill ID already belongs to different input.",
            ));
        }
        Ok((200, saved.retired_view(&self.command_id)))
    }
}

impl BackfillRetry {
    pub(crate) fn replay(self, app: &Tempo) -> Result<(u16, Value), AppError> {
        check_id(&self.command_id)?;
        check_id(&self.job_id)?;
        require_dataset(app, &self.dataset_id)?;
        if let Some(saved) = app.read_backfill(&self.dataset_id, &self.job_id)? {
            if saved.id != self.job_id || saved.request.dataset_id != self.dataset_id {
                return Err(invalid(
                    "The saved historical job has a different identity.",
                ));
            }
            if saved.retries.contains(&self.command_id) {
                return Ok((200, saved.retired_view(&self.command_id)));
            }
        }
        Ok(not_accepted(
            &self.dataset_id,
            &self.job_id,
            &self.command_id,
        ))
    }
}

fn require_dataset(app: &Tempo, dataset: &DatasetId) -> Result<(), AppError> {
    if &app.game_dataset()? != dataset {
        return Err(AppError::new(
            "stale_dataset",
            "The active dataset changed.",
        ));
    }
    Ok(())
}

fn not_accepted(dataset: &DatasetId, job_id: &str, command_id: &str) -> (u16, Value) {
    (
        410,
        json!({
            "status": "error", "code": "retired_feature",
            "message": "This request was not accepted. Historical Games backfill has been retired.",
            "retryable": false, "dataset_id": dataset, "job_id": job_id, "command_id": command_id,
        }),
    )
}

fn invalid(message: &str) -> AppError {
    AppError::new("invalid_backfill", message)
}
