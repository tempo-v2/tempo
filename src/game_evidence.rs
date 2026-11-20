//! Answer-safe game views and the local host's explicit refresh job.

use crate::adapters::public_games::{FetchedGames, Provider, PublicGames};
use crate::{AppError, GameRange, Tempo};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
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

type RefreshResult = (GameRefresh, Result<FetchedGames, AppError>);

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

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RefreshAcknowledgement {
    Running {
        dataset_id: DatasetId,
        command_id: String,
    },
    Complete {
        dataset_id: DatasetId,
        command_id: String,
        report: GameReport,
    },
}

#[derive(Debug, PartialEq, Eq)]
struct PendingRefresh {
    dataset_id: DatasetId,
    command_id: String,
    fingerprint: String,
}

impl PendingRefresh {
    fn running(&self) -> RefreshAcknowledgement {
        RefreshAcknowledgement::Running {
            dataset_id: self.dataset_id.clone(),
            command_id: self.command_id.clone(),
        }
    }

    fn failed(&self, error: String) -> Value {
        json!({
            "status": "failed",
            "dataset_id": self.dataset_id,
            "command_id": self.command_id,
            "error": error,
        })
    }
}

/// A single serial worker keeps provider I/O off the Study request loop.
pub struct GameJobs {
    commands: SyncSender<GameRefresh>,
    results: Receiver<RefreshResult>,
    status: Value,
    pending: Option<PendingRefresh>,
}

impl GameJobs {
    pub fn start() -> std::io::Result<Self> {
        let _diagnostic = crate::diagnostics::Span::new("refresh.initialize");
        let (commands, requests) = sync_channel::<GameRefresh>(1);
        let (replies, results) = sync_channel(1);
        std::thread::Builder::new()
            .name("tempo-game-refresh".into())
            .spawn(move || {
                let mut providers = PublicGames::default();
                while let Ok(refresh) = requests.recv() {
                    let mut trace = crate::diagnostics::Span::new("refresh.fetch");
                    let range = GameRange {
                        from_ms: refresh.from_ms,
                        to_ms: refresh.to_ms,
                    };
                    let fetched = providers.fetch(refresh.source, &refresh.account, &range);
                    trace.set_outcome(if fetched.is_ok() { "returned" } else { "error" });
                    if replies.send((refresh, fetched)).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            commands,
            results,
            status: json!({"status":"idle"}),
            pending: None,
        })
    }

    pub fn active(&self) -> bool {
        self.pending.is_some()
    }

    /// Acknowledge this exact command without settling or replacing an unrelated job.
    pub fn refresh(
        &mut self,
        app: &Tempo,
        request: GameRefresh,
        allow_fetch: bool,
    ) -> Result<RefreshAcknowledgement, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("refresh.dispatch");
        check_id(&request.command_id)?;
        if app.game_dataset()? != request.dataset_id {
            return Err(AppError::new(
                "stale_dataset",
                "The active dataset changed.",
            ));
        }
        let key = fingerprint("refresh", &request)?;
        if let Some(report) = app.replay_game(&request.dataset_id, &request.command_id, &key)? {
            return Ok(RefreshAcknowledgement::Complete {
                dataset_id: request.dataset_id,
                command_id: request.command_id,
                report,
            });
        }
        if let Some(pending) = &self.pending {
            if pending.dataset_id == request.dataset_id && pending.command_id == request.command_id
            {
                if pending.fingerprint == key {
                    return Ok(pending.running());
                }
                return Err(AppError::new(
                    "store_conflict",
                    "Game command ID was reused with different inputs.",
                ));
            }
            return Err(AppError::new(
                "game_refresh_busy",
                "A game refresh is already running.",
            ));
        }
        if !allow_fetch {
            return Err(AppError::new(
                "game_refresh_busy",
                "Historical Games import is running.",
            ));
        }
        let pending = PendingRefresh {
            dataset_id: request.dataset_id.clone(),
            command_id: request.command_id.clone(),
            fingerprint: key,
        };
        self.commands.try_send(request).map_err(|_| {
            AppError::new(
                "game_refresh_unavailable",
                "The background refresh worker is unavailable.",
            )
        })?;
        let reply = pending.running();
        self.status = json!(reply);
        self.pending = Some(pending);
        Ok(reply)
    }

    /// Apply fetched records on the single application owner, after network I/O.
    pub fn poll(&mut self, app: &mut Tempo) -> Value {
        let _diagnostic = crate::diagnostics::Span::new("refresh.apply");
        if let Some(pending) = &self.pending {
            self.status = match self.results.try_recv() {
                Ok((request, fetched)) => match fetched.and_then(|fetched| {
                    app.ingest_provider_games(
                        &pending.dataset_id,
                        &pending.command_id,
                        &pending.fingerprint,
                        &request.account,
                        fetched,
                        None,
                    )
                }) {
                    Ok(report) => json!(RefreshAcknowledgement::Complete {
                        dataset_id: pending.dataset_id.clone(),
                        command_id: pending.command_id.clone(),
                        report,
                    }),
                    Err(error) => pending.failed(error.message),
                },
                Err(TryRecvError::Empty) => return self.status.clone(),
                Err(TryRecvError::Disconnected) => {
                    pending.failed("The background refresh worker stopped.".into())
                }
            };
            self.pending = None;
        }
        self.status.clone()
    }
}

#[cfg(test)]
mod tests {
    mod refresh_replay;
}
