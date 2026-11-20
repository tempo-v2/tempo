//! Dispatch durable position tasks through the existing detector host.
use crate::application::tactical_analysis::analysis_version;
use crate::game_detection::{
    DetectionCompletion, DetectionRequest, DetectionStatus, GameDetection,
};
use crate::{AppError, Tempo};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempo_chess::{DetectorDescriptor, DetectorIdentity, parse_played_games};
use tempo_store::{SqliteStore, TacticalPositionTask};
use tempo_types::DatasetId;

const IDLE: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TacticalRead {
    pub(crate) dataset_id: DatasetId,
    #[serde(default)]
    pub(crate) before: Option<u64>,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum TacticalCommand {
    Enqueue {
        dataset_id: DatasetId,
        source: String,
        game_id: String,
        learner: String,
    },
    Pause {
        dataset_id: DatasetId,
    },
    Resume {
        dataset_id: DatasetId,
    },
    Retry {
        dataset_id: DatasetId,
    },
}

pub(crate) struct TacticalJobs {
    descriptor: Option<DetectorDescriptor>,
    analyses: Vec<DetectorIdentity>,
    version: String,
    active: Option<(TacticalPositionTask, String)>,
    ready: Option<(TacticalPositionTask, DetectionCompletion)>,
    next_check: Instant,
    last_study: Option<Instant>,
    idle_on_ac: Arc<AtomicBool>,
    run: u128,
    sequence: u64,
    last_error: Option<String>,
    continuing: Option<i64>,
}

impl TacticalJobs {
    pub(crate) fn new(detection: &GameDetection, selected: &[&str]) -> Self {
        let configured = selected
            .iter()
            .map(|id| detection.tactical_descriptor(id))
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default();
        let descriptor = configured.first().cloned();
        let analyses: Vec<_> = configured
            .iter()
            .map(DetectorDescriptor::identity)
            .collect();
        let version = analysis_version(&analyses);
        let idle_on_ac = Arc::new(AtomicBool::new(false));
        if descriptor.is_some() {
            let sensor = Arc::downgrade(&idle_on_ac);
            std::thread::spawn(move || {
                loop {
                    let ready = machine_idle_on_ac();
                    let Some(state) = sensor.upgrade() else { break };
                    state.store(ready, Ordering::Relaxed);
                    drop(state);
                    std::thread::sleep(Duration::from_millis(350));
                }
            });
        }
        Self {
            descriptor,
            analyses,
            version,
            active: None,
            ready: None,
            next_check: Instant::now(),
            last_study: None,
            idle_on_ac,
            run: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            sequence: 0,
            last_error: None,
            continuing: None,
        }
    }

    pub(crate) fn study_activity(&mut self, detection: &mut GameDetection) {
        self.last_study = Some(Instant::now());
        detection.cancel_background();
    }

    #[cfg(test)]
    pub(crate) fn force_idle_for_test(&mut self) {
        self.idle_on_ac = Arc::new(AtomicBool::new(true));
    }

    fn idle(&self) -> bool {
        self.idle_on_ac.load(Ordering::Relaxed)
            && self.last_study.is_none_or(|event| event.elapsed() >= IDLE)
            && !crate::stockfish::foreground_waiting()
    }

    pub(crate) fn poll(
        &mut self,
        app: &mut Tempo,
        store: &SqliteStore,
        detection: &mut GameDetection,
    ) {
        if self.descriptor.is_none() || Instant::now() < self.next_check {
            return;
        }
        self.next_check = Instant::now() + Duration::from_millis(100);
        if let Err(error) = self.advance(app, store, detection) {
            self.last_error = Some(error.message);
            self.next_check = Instant::now() + Duration::from_secs(2);
        }
    }

    fn advance(
        &mut self,
        app: &mut Tempo,
        store: &SqliteStore,
        detection: &mut GameDetection,
    ) -> Result<(), AppError> {
        let dataset = app.game_dataset()?;
        let run_allowed = self.idle() && !store.analysis_paused(&dataset)?;
        if !run_allowed {
            detection.cancel_background();
        }
        if let Some((_, id)) = &self.active
            && detection.idle()
            && let Some(result) = detection.completion(app, id)
        {
            let (task, _) = self.active.take().expect("active position");
            if !matches!(
                result.status,
                DetectionStatus::Cancelled | DetectionStatus::SourceUnavailable
            ) {
                self.ready = Some((task, result));
            }
        }
        if run_allowed && let Some((task, result)) = &self.ready {
            match app.accept_tactical_analysis(&task.game, result) {
                Ok(_) => self.last_error = None,
                Err(error) if matches!(error.code.as_str(), "stale_source" | "stale_dataset") => {}
                Err(error) if !error.retryable => {
                    app.record_tactical_failure(&task.game, result, &error)?;
                    self.last_error = Some(error.message);
                }
                Err(error) => return Err(error),
            }
            self.ready = None;
        }
        if !run_allowed || self.ready.is_some() || self.active.is_some() || !detection.idle() {
            return Ok(());
        }
        let descriptor = self.descriptor.as_ref().expect("configured detector");
        let version = &self.version;
        let continuing = store.next_position_analysis(&dataset, version, self.continuing)?;
        let task = if continuing
            .as_ref()
            .is_some_and(|task| Some(task.game.cursor.ordinal) == self.continuing)
        {
            continuing
        } else {
            if let Some(game) = store.next_analysis_enrollment(&dataset, version)? {
                let learner = crate::application::tactical_analysis::game_learner(&game)?;
                let mut parsed = parse_played_games(&game.game.pgn)
                    .map_err(|error| crate::app_error::domain("invalid_game", error))?;
                let played = parsed
                    .pop()
                    .filter(|_| parsed.is_empty())
                    .and_then(Result::ok)
                    .ok_or_else(|| AppError::new("invalid_game", "Expected one accepted game."))?;
                let plies: Vec<_> = played
                    .timeline()
                    .occurrences()
                    .iter()
                    .filter(|position| {
                        position.side_to_move() == learner && position.next_move().is_some()
                    })
                    .map(|position| position.offset() as u32)
                    .collect();
                store.expand_analysis_enrollment(
                    &dataset,
                    &game,
                    version,
                    &self.analyses,
                    &plies,
                )?;
            }
            store.next_position_analysis(&dataset, version, None)?
        };
        let Some(task) = task else {
            self.continuing = None;
            self.next_check = Instant::now() + Duration::from_secs(2);
            return Ok(());
        };
        self.sequence += 1;
        let request_id = format!("position-{}-{}", self.run, self.sequence);
        let reply = detection.start_work(
            app,
            DetectionRequest {
                schema_version: 1,
                request_id: request_id.clone(),
                dataset_id: dataset,
                source: task.game.game.source.clone(),
                game_id: task.game.game.id.clone(),
                learner: task.game.learner.clone(),
                detector_id: descriptor.id.clone(),
            },
            Some(task.run),
            &task.analyses,
        );
        if reply.status != 202 {
            return Err(AppError::new(
                "tactical_dispatch_failed",
                "The position could not start analysis.",
            ));
        }
        self.continuing = Some(task.game.cursor.ordinal);
        self.active = Some((task, request_id));
        Ok(())
    }

    pub(crate) fn command(
        &mut self,
        store: &SqliteStore,
        detection: &mut GameDetection,
        command: TacticalCommand,
    ) -> Result<Value, AppError> {
        match command {
            TacticalCommand::Enqueue {
                dataset_id,
                source,
                game_id,
                learner,
            } => store.queue_game_analysis(&dataset_id, &source, &game_id, &learner)?,
            TacticalCommand::Pause { dataset_id } => {
                store.set_analysis_paused(&dataset_id, true)?;
                detection.cancel_background();
            }
            TacticalCommand::Resume { dataset_id } => {
                store.set_analysis_paused(&dataset_id, false)?
            }
            TacticalCommand::Retry { dataset_id } => store.retry_position_analysis(&dataset_id)?,
        }
        self.next_check = Instant::now();
        Ok(json!({"accepted":true}))
    }

    pub(crate) fn read(
        &self,
        app: &Tempo,
        store: &SqliteStore,
        read: TacticalRead,
    ) -> Result<Value, AppError> {
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
        let mut reply =
            app.tactical_history(&read.dataset_id, descriptor, &self.version, read.before)?;
        reply["schema_version"] = json!(1);
        reply["dataset_id"] = json!(read.dataset_id);
        reply["status"] = json!(if self.active.is_some() || self.ready.is_some() {
            "running"
        } else {
            "ready"
        });
        reply["games"] = json!(store.recent_analysis_games(&read.dataset_id, &self.version)?);
        reply["paused"] = json!(store.analysis_paused(&read.dataset_id)?);
        reply["waiting_for_idle"] = json!(!self.idle());
        reply["detector"] = json!(descriptor);
        reply["analysis_version"] = json!(self.version);
        reply["last_error"] = json!(self.last_error);
        reply["active_game"] = json!(self.active.as_ref().map(|(task, _)| json!({
            "source":task.game.game.source,"game_id":task.game.game.id,"learner":task.game.learner
        })));
        Ok(reply)
    }
}

#[cfg(target_os = "macos")]
fn machine_idle_on_ac() -> bool {
    use std::process::Command;
    let idle = Command::new("/usr/sbin/ioreg")
        .args(["-r", "-c", "IOHIDSystem", "-d", "1"])
        .output();
    let idle = idle
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| {
                    line.trim()
                        .strip_prefix("\"HIDIdleTime\" = ")?
                        .trim()
                        .parse::<u64>()
                        .ok()
                })
        });
    if idle.is_none_or(|ns| Duration::from_nanos(ns) < IDLE) {
        return false;
    }
    Command::new("/usr/bin/pmset")
        .args(["-g", "batt"])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).lines().next()
                    == Some("Now drawing from 'AC Power'")
        })
}

#[cfg(not(target_os = "macos"))]
fn machine_idle_on_ac() -> bool {
    false
}
