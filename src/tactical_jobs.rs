//! Resume background tactical work through the existing detector host.
//! Game acceptance is durable; a missed wake-up or restart is recovered by the
//! bounded pending-game read. Result reads never perform training mutations.
use crate::game_detection::{DetectionCompletion, DetectionRequest, GameDetection};
use crate::{AppError, Tempo, application::tactical_analysis::analysis_version};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempo_chess::DetectorDescriptor;
use tempo_store::TacticalGame;
use tempo_types::DatasetId;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TacticalRead {
    pub(crate) dataset_id: DatasetId,
    #[serde(default)]
    pub(crate) before: Option<u64>,
}

struct Active {
    dataset: DatasetId,
    request_id: String,
    game: TacticalGame,
}

pub(crate) struct TacticalJobs {
    descriptor: Option<DetectorDescriptor>,
    active: Option<Active>,
    ready: Option<(TacticalGame, DetectionCompletion)>,
    next_check: Instant,
    run: String,
    sequence: u64,
    last_error: Option<String>,
}

impl TacticalJobs {
    pub(crate) fn new(detection: &GameDetection) -> Self {
        Self {
            descriptor: detection.descriptor("hanging-material"),
            active: None,
            ready: None,
            next_check: Instant::now(),
            run: format!(
                "{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            sequence: 0,
            last_error: None,
        }
    }

    pub(crate) fn poll(&mut self, app: &mut Tempo, detection: &mut GameDetection) {
        if self.descriptor.is_none() || Instant::now() < self.next_check {
            return;
        }
        self.next_check = Instant::now() + Duration::from_millis(100);
        let result = self.advance(app, detection);
        if let Err(error) = result {
            self.last_error = Some(error.message.clone());
            self.next_check = Instant::now() + Duration::from_secs(2);
            eprintln!("Tempo tactical analysis: {}", error.message);
        }
    }

    fn advance(&mut self, app: &mut Tempo, detection: &mut GameDetection) -> Result<(), AppError> {
        let dataset = app.game_dataset()?;
        if self
            .active
            .as_ref()
            .is_some_and(|job| job.dataset != dataset)
        {
            self.active = None;
        }
        if self
            .ready
            .as_ref()
            .is_some_and(|(_, result)| result.request.dataset_id != dataset)
        {
            self.ready = None;
        }
        if let Some(active) = &self.active
            && let Some(result) = detection.completion(app, &active.request_id)
        {
            let active = self.active.take().expect("active request");
            self.ready = Some((active.game, result));
        }
        if let Some((game, result)) = &self.ready {
            match app.accept_tactical_analysis(game, result) {
                Ok(_) => {
                    self.ready = None;
                    self.last_error = None;
                }
                Err(error) if matches!(error.code.as_str(), "stale_source" | "stale_dataset") => {
                    self.ready = None;
                }
                Err(error)
                    if !error.retryable
                        && matches!(
                            error.code.as_str(),
                            "invalid_game"
                                | "invalid_tactical_result"
                                | "invalid_tactical_puzzle"
                                | "learning_error"
                                | "store_invalid"
                        ) =>
                {
                    // The rejected composition has no durable effects. Save a
                    // truthful failure before releasing this completion, so a
                    // restart resumes the rest of the archive rather than
                    // retrying one deterministic failure forever. If this write
                    // fails, `ready` still owns the original completion.
                    app.record_tactical_failure(game, result, &error)?;
                    self.last_error = Some(error.message);
                    self.ready = None;
                }
                Err(error) => return Err(error),
            }
        }
        if self.active.is_some() || !detection.idle() {
            return Ok(());
        }
        let descriptor = self.descriptor.as_ref().expect("configured detector");
        let Some(game) = app.pending_tactical_game(descriptor)? else {
            self.next_check = Instant::now() + Duration::from_secs(2);
            return Ok(());
        };
        self.sequence += 1;
        let request_id = format!("automatic-tactic-{}-{}", self.run, self.sequence);
        let reply = detection.start(
            app,
            DetectionRequest {
                schema_version: 1,
                request_id: request_id.clone(),
                dataset_id: dataset.clone(),
                source: game.game.source.clone(),
                game_id: game.game.id.clone(),
                learner: game.learner.clone(),
                detector_id: descriptor.id.clone(),
            },
        );
        if reply.status == 202 {
            self.active = Some(Active {
                dataset,
                request_id,
                game,
            });
            Ok(())
        } else {
            Err(AppError::new(
                "tactical_dispatch_failed",
                "The pending game could not start tactical analysis.",
            ))
        }
    }

    pub(crate) fn read(&self, app: &Tempo, read: TacticalRead) -> Result<Value, AppError> {
        if app.game_dataset()? != read.dataset_id {
            return Err(AppError::new(
                "stale_dataset",
                "Reload the current game archive.",
            ));
        }
        let Some(descriptor) = &self.descriptor else {
            return Ok(
                json!({"schema_version":1,"dataset_id":read.dataset_id,"status":"unavailable",
                "message":"Hanging-material analysis needs the configured Stockfish and Python chess environment."}),
            );
        };
        let mut reply = app.tactical_history(&read.dataset_id, descriptor, read.before)?;
        reply["schema_version"] = json!(1);
        reply["dataset_id"] = json!(read.dataset_id);
        reply["status"] = json!(if self.active.is_some() || self.ready.is_some() {
            "running"
        } else {
            "ready"
        });
        reply["detector"] = json!(descriptor);
        reply["analysis_version"] = json!(analysis_version(descriptor));
        reply["last_error"] = json!(self.last_error);
        reply["active_game"] = json!(self.active.as_ref().map(|active| json!({
            "source":active.game.game.source,"game_id":active.game.game.id,"learner":active.game.learner
        })));
        Ok(reply)
    }
}
