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

/// A single serial worker keeps provider I/O off the Study request loop.
pub struct GameJobs {
    commands: SyncSender<GameRefresh>,
    results: Receiver<RefreshResult>,
    status: Value,
    pending: Option<(String, String)>,
}

impl GameJobs {
    pub fn start() -> std::io::Result<Self> {
        let (commands, requests) = sync_channel::<GameRefresh>(1);
        let (replies, results) = sync_channel(1);
        std::thread::Builder::new()
            .name("tempo-game-refresh".into())
            .spawn(move || {
                let mut providers = PublicGames::default();
                while let Ok(refresh) = requests.recv() {
                    let range = GameRange {
                        from_ms: refresh.from_ms,
                        to_ms: refresh.to_ms,
                    };
                    let fetched = providers.fetch(refresh.source, &refresh.account, &range);
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

    pub fn refresh(
        &mut self,
        app: &mut Tempo,
        request: GameRefresh,
        allow_fetch: bool,
    ) -> Result<Value, AppError> {
        self.poll(app);
        check_id(&request.command_id)?;
        let key = fingerprint("refresh", &request)?;
        if let Some((id, pending_key)) = &self.pending {
            if id == &request.command_id && pending_key == &key {
                return Ok(self.status.clone());
            }
            return Err(AppError::new(
                "game_refresh_busy",
                "A game refresh is already running.",
            ));
        }
        if let Some(report) = app.replay_game(&request.dataset_id, &request.command_id, &key)? {
            self.status =
                json!({"status":"complete","command_id":request.command_id,"report":report});
            return Ok(self.status.clone());
        }
        if app.game_dataset()? != request.dataset_id {
            return Err(AppError::new(
                "stale_dataset",
                "The active dataset changed.",
            ));
        }
        if !allow_fetch {
            return Err(AppError::new(
                "game_refresh_busy",
                "Historical Games import is running.",
            ));
        }
        let id = request.command_id.clone();
        self.commands.try_send(request).map_err(|_| {
            AppError::new(
                "game_refresh_unavailable",
                "The background refresh worker is unavailable.",
            )
        })?;
        self.status = json!({"status":"running","command_id":id});
        self.pending = Some((id, key));
        Ok(self.status.clone())
    }

    /// Apply fetched records on the single application owner, after network I/O.
    pub fn poll(&mut self, app: &mut Tempo) -> Value {
        if let Some((_, key)) = &self.pending {
            self.status = match self.results.try_recv() {
                Ok((request, fetched)) => {
                    let key = key.clone();
                    self.pending = None;
                    match fetched.and_then(|fetched| {
                        app.ingest_provider_games(
                            &request.dataset_id,
                            &request.command_id,
                            &key,
                            &request.account,
                            fetched,
                            None,
                        )
                    }) {
                        Ok(report) => {
                            json!({"status":"complete","command_id":request.command_id,"report":report})
                        }
                        Err(error) => {
                            json!({"status":"failed","command_id":request.command_id,"error":error.message})
                        }
                    }
                }
                Err(TryRecvError::Empty) => return self.status.clone(),
                Err(TryRecvError::Disconnected) => {
                    self.pending = None;
                    json!({"status":"failed","error":"The background refresh worker stopped."})
                }
            };
        }
        self.status.clone()
    }
}
