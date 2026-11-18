//! Answer-safe game views and the local host's explicit refresh job.

use crate::adapters::public_games::{FetchedGames, Provider, PublicGames};
use crate::{AppError, GameRange, Tempo};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use tempo_store::GameSummary;
use tempo_types::DatasetId;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameImport {
    pub learner: String,
    pub repertoire_id: String,
    pub pgn: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameRefresh {
    source: Provider,
    account: String,
    from_ms: u64,
    to_ms: u64,
    repertoire_id: String,
}

#[derive(Default, Serialize)]
pub struct GameReport {
    pub fetched: usize,
    pub new_games: usize,
    pub duplicates: usize,
    pub matches: usize,
    pub deviations: usize,
    pub errors: Vec<String>,
    pub results: Vec<GameResult>,
}

#[derive(Serialize)]
pub struct GameResult {
    #[serde(flatten)]
    pub summary: GameSummary,
    pub effect: GameEffect,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GameEffect {
    RecordedOnly,
}

type RefreshResult = (DatasetId, GameRefresh, Result<FetchedGames, AppError>);

/// A single serial worker keeps provider I/O off the Study request loop.
pub struct GameJobs {
    commands: SyncSender<(DatasetId, GameRefresh)>,
    results: Receiver<RefreshResult>,
    status: Value,
    running: bool,
}

impl GameJobs {
    pub fn start() -> std::io::Result<Self> {
        let (commands, requests) = sync_channel::<(DatasetId, GameRefresh)>(1);
        let (replies, results) = sync_channel(1);
        std::thread::Builder::new()
            .name("tempo-game-refresh".into())
            .spawn(move || {
                let mut providers = PublicGames::default();
                while let Ok((dataset, refresh)) = requests.recv() {
                    let range = GameRange {
                        from_ms: refresh.from_ms,
                        to_ms: refresh.to_ms,
                    };
                    let fetched = providers.fetch(refresh.source, &refresh.account, &range);
                    if replies.send((dataset, refresh, fetched)).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            commands,
            results,
            status: json!({"status":"idle"}),
            running: false,
        })
    }

    pub fn refresh(&mut self, app: &mut Tempo, request: GameRefresh) -> Result<Value, AppError> {
        self.poll(app);
        if self.running {
            return Err(AppError::new(
                "game_refresh_busy",
                "A game refresh is already running.",
            ));
        }
        let dataset = app.game_repertoire_dataset(&request.repertoire_id)?;
        self.commands.try_send((dataset, request)).map_err(|_| {
            AppError::new(
                "game_refresh_unavailable",
                "The background refresh worker is unavailable.",
            )
        })?;
        self.running = true;
        self.status = json!({"status":"running"});
        Ok(self.status.clone())
    }

    /// Apply fetched records on the single application owner, after network I/O.
    pub fn poll(&mut self, app: &mut Tempo) -> Value {
        if self.running {
            self.status = match self.results.try_recv() {
                Ok((dataset, request, fetched)) => {
                    self.running = false;
                    match fetched.and_then(|fetched| {
                        app.ingest_provider_games(
                            &dataset,
                            &request.repertoire_id,
                            &request.account,
                            fetched,
                        )
                    }) {
                        Ok(report) => json!({"status":"complete","report":report}),
                        Err(error) => json!({"status":"failed","error":error.message}),
                    }
                }
                Err(TryRecvError::Empty) => return self.status.clone(),
                Err(TryRecvError::Disconnected) => {
                    self.running = false;
                    json!({"status":"failed","error":"The background refresh worker stopped."})
                }
            };
        }
        self.status.clone()
    }
}
