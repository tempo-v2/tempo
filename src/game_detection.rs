//! Ephemeral, addressed execution of one explicitly requested stored-game scan.
use crate::{AppError, Tempo};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    time::{Duration, Instant},
};
use tempo_chess::{
    DetectionControl, DetectionOutcome, DetectionStop, DetectorDescriptor, GameDetectionInput,
    GameDetector, LearnerColor, ValidatedMoment, parse_played_games, validate_detected_moments,
};
use tempo_types::DatasetId;

const VERSION: u8 = 1;

#[derive(Clone, Copy)]
pub struct AnalysisOptions {
    pub deadline: Duration,
    pub retained_results: usize,
}

impl Default for AnalysisOptions {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(10),
            retained_results: 48,
        }
    }
}

/// Fixed for one host lifetime. Capture descriptors at registration.
#[derive(Default)]
pub struct DetectorRegistry(BTreeMap<String, RegisteredDetector>);

#[derive(Clone)]
struct RegisteredDetector {
    descriptor: DetectorDescriptor,
    detector: Arc<dyn GameDetector>,
}

impl DetectorRegistry {
    pub fn register(&mut self, detector: impl GameDetector + 'static) -> Result<(), AppError> {
        let descriptor = detector.descriptor();
        descriptor
            .validate()
            .map_err(|_| AppError::new("invalid_detector", "Invalid detector descriptor."))?;
        if self.0.contains_key(&descriptor.id) {
            return Err(AppError::new(
                "duplicate_detector",
                "Duplicate detector ID.",
            ));
        }
        self.0.insert(
            descriptor.id.clone(),
            RegisteredDetector {
                descriptor,
                detector: Arc::new(detector),
            },
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DetectionRequest {
    schema_version: u8,
    request_id: String,
    pub(crate) dataset_id: DatasetId,
    pub(crate) source: String,
    pub(crate) game_id: String,
    pub(crate) learner: String,
    detector_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DetectionRead {
    schema_version: u8,
    request_id: String,
    dataset_id: DatasetId,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum DetectionStatus {
    Running,
    Complete {
        learner_color: LearnerColor,
        occurrence_count: usize,
        input_key: String,
        analysis_key: String,
        findings: Vec<DetectionFinding>,
    },
    Cancelled,
    TimedOut,
    Unsupported,
    InvalidResult,
    Failed,
    SourceUnavailable,
}

#[derive(Serialize)]
struct DetectionFinding {
    finding_id: String,
    #[serde(flatten)]
    moment: ValidatedMoment,
}

pub(crate) struct DetectionReply {
    pub(crate) status: u16,
    pub(crate) body: Value,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AcceptedDetectionSource {
    pub(crate) pgn: String,
    pub(crate) color: LearnerColor,
}

struct Stop {
    deadline: Instant,
    cancelled: AtomicBool,
}

impl DetectionControl for Stop {
    fn check(&self) -> Result<(), DetectionStop> {
        if self.cancelled.load(Ordering::Relaxed) {
            Err(DetectionStop::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(DetectionStop::Deadline)
        } else {
            Ok(())
        }
    }
}

struct Work {
    diagnostic_context: Option<String>,
    request: DetectionRequest,
    source: Arc<AcceptedDetectionSource>,
    detector: RegisteredDetector,
    stop: Arc<Stop>,
    reply: SyncSender<DetectionStatus>,
}

struct Entry {
    request: DetectionRequest,
    source: Arc<AcceptedDetectionSource>,
    descriptor: DetectorDescriptor,
    status: DetectionStatus,
}

impl Entry {
    fn reply(&self) -> DetectionReply {
        let mut body = json!({
            "schema_version": VERSION,
            "request_id": self.request.request_id,
            "dataset_id": self.request.dataset_id,
            "source": self.request.source,
            "game_id": self.request.game_id,
            "detector": descriptor_json(&self.descriptor),
        });
        let status = serde_json::to_value(&self.status).expect("serializable detection status");
        body.as_object_mut()
            .unwrap()
            .extend(status.as_object().unwrap().clone());
        DetectionReply {
            status: if matches!(self.status, DetectionStatus::Running) {
                202
            } else {
                200
            },
            body,
        }
    }

    fn validate_source(&mut self, app: &Tempo) {
        match app.accepted_detection_source(&self.request) {
            Ok(source) if source == *self.source => {}
            _ => self.status = DetectionStatus::SourceUnavailable,
        }
    }
}

struct Active {
    diagnostic_context: Option<String>,
    entry: Entry,
    stop: Arc<Stop>,
    reply: Receiver<DetectionStatus>,
}

pub(crate) struct GameDetection {
    registry: DetectorRegistry,
    options: AnalysisOptions,
    work: SyncSender<Work>,
    active: Option<Active>,
    retained: VecDeque<Entry>,
}

impl GameDetection {
    pub(crate) fn new(
        registry: DetectorRegistry,
        options: AnalysisOptions,
    ) -> std::io::Result<Self> {
        let _diagnostic = crate::diagnostics::Span::new("detection.initialize");
        let (work, incoming) = mpsc::sync_channel::<Work>(1);
        std::thread::Builder::new()
            .name("tempo-game-detection".into())
            .spawn(move || {
                while let Ok(task) = incoming.recv() {
                    crate::diagnostics::with_context(task.diagnostic_context.clone(), || {
                        let mut trace = crate::diagnostics::Span::new("detection.worker");
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            detect(&task)
                        }))
                        .unwrap_or(DetectionStatus::Failed);
                        trace.set_outcome(diagnostic_status(&result));
                        let _ = task.reply.send(result);
                    });
                }
            })?;
        Ok(Self {
            registry,
            options,
            work,
            active: None,
            retained: VecDeque::new(),
        })
    }

    pub(crate) fn discovery(&self) -> DetectionReply {
        let detectors: Vec<_> = self
            .registry
            .0
            .values()
            .map(|registered| descriptor_json(&registered.descriptor))
            .collect();
        DetectionReply {
            status: 200,
            body: json!({
                "schema_version": VERSION,
                "status": if detectors.is_empty() { "unconfigured" } else { "available" },
                "detectors": detectors,
            }),
        }
    }

    pub(crate) fn start(&mut self, app: &Tempo, request: DetectionRequest) -> DetectionReply {
        let _diagnostic = crate::diagnostics::Span::new("detection.start");
        let error =
            |status, code| addressed_error(status, code, &request.request_id, &request.dataset_id);
        if request.schema_version != VERSION
            || [
                &request.request_id,
                &request.dataset_id.0,
                &request.source,
                &request.game_id,
                &request.learner,
                &request.detector_id,
            ]
            .iter()
            .any(|value| value.trim().is_empty())
        {
            return error(400, "invalid_request");
        }
        self.poll(app);
        self.revalidate_retained(app, &request.request_id);
        if let Some(entry) = self.lookup(&request.request_id) {
            return if entry.request == request {
                entry.reply()
            } else {
                error(409, "conflict")
            };
        }
        let Some(detector) = self.registry.0.get(&request.detector_id).cloned() else {
            let code = if self.registry.0.is_empty() {
                "unconfigured"
            } else {
                "unknown_detector"
            };
            return error(404, code);
        };
        if self.active.is_some() {
            crate::diagnostics::emit(
                "detection.busy",
                json!({"operation":"detection.start","outcome":"busy"}),
            );
            return error(409, "busy");
        }
        let source = match app.accepted_detection_source(&request) {
            Ok(source) => Arc::new(source),
            Err(error) if error.code == "invalid_perspective" => {
                return addressed_error(
                    400,
                    "invalid_perspective",
                    &request.request_id,
                    &request.dataset_id,
                );
            }
            Err(_) => return error(404, "source_unavailable"),
        };
        let Some(deadline) = Instant::now().checked_add(self.options.deadline) else {
            return error(503, "unavailable");
        };
        let stop = Arc::new(Stop {
            deadline,
            cancelled: AtomicBool::new(false),
        });
        let (reply, result) = mpsc::sync_channel(1);
        let entry = Entry {
            request: request.clone(),
            source: source.clone(),
            descriptor: detector.descriptor.clone(),
            status: DetectionStatus::Running,
        };
        let diagnostic_context = crate::diagnostics::capture_context();
        if self
            .work
            .try_send(Work {
                diagnostic_context: diagnostic_context.clone(),
                request,
                source,
                detector,
                stop: stop.clone(),
                reply,
            })
            .is_err()
        {
            return addressed_error(
                503,
                "unavailable",
                &entry.request.request_id,
                &entry.request.dataset_id,
            );
        }
        let delivery = entry.reply();
        self.active = Some(Active {
            diagnostic_context,
            entry,
            stop,
            reply: result,
        });
        crate::diagnostics::emit(
            "detection.queued",
            json!({"operation":"detection.start","outcome":"queued"}),
        );
        delivery
    }

    pub(crate) fn read(&mut self, app: &Tempo, request: DetectionRead) -> DetectionReply {
        let _diagnostic = crate::diagnostics::Span::new("detection.read");
        let error =
            |status, code| addressed_error(status, code, &request.request_id, &request.dataset_id);
        if request.schema_version != VERSION
            || request.request_id.trim().is_empty()
            || request.dataset_id.0.trim().is_empty()
        {
            return error(400, "invalid_request");
        }
        self.poll(app);
        self.revalidate_retained(app, &request.request_id);
        match self.lookup(&request.request_id) {
            Some(entry) if entry.request.dataset_id == request.dataset_id => entry.reply(),
            Some(_) => error(409, "stale"),
            None => error(404, "expired"),
        }
    }

    fn lookup(&self, id: &str) -> Option<&Entry> {
        self.active
            .as_ref()
            .map(|active| &active.entry)
            .into_iter()
            .chain(self.retained.iter())
            .find(|entry| entry.request.request_id == id)
    }

    pub(crate) fn poll(&mut self, app: &Tempo) {
        let _diagnostic = crate::diagnostics::Span::new("detection.poll");
        let Some(active) = &mut self.active else {
            return;
        };
        if app.game_dataset().as_ref().ok() != Some(&active.entry.request.dataset_id) {
            active.entry.status = DetectionStatus::SourceUnavailable;
            active.stop.cancelled.store(true, Ordering::Relaxed);
        }
        let result = match active.reply.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => {
                if matches!(active.entry.status, DetectionStatus::Running)
                    && let Err(stop) = active.stop.check()
                {
                    active.entry.status = stopped(stop);
                    crate::diagnostics::with_context(active.diagnostic_context.clone(), || {
                        crate::diagnostics::emit(
                            "detection.deadline",
                            json!({
                                "operation":"detection.poll", "outcome": diagnostic_status(&active.entry.status)
                            }),
                        );
                    });
                }
                return;
            }
            Err(TryRecvError::Disconnected) => DetectionStatus::Failed,
        };
        let active = self.active.take().unwrap();
        crate::diagnostics::with_context(active.diagnostic_context, || {
            crate::diagnostics::emit(
                "detection.acknowledged",
                json!({
                    "operation":"detection.poll",
                    "outcome": if matches!(active.entry.status, DetectionStatus::Running) { "accepted_result" } else { "late_result_discarded" }
                }),
            );
        });
        let mut finished = active.entry;
        if matches!(finished.status, DetectionStatus::Running) {
            finished.status = result;
        }
        finished.validate_source(app);
        self.retained.push_back(finished);
        if self.retained.len() > self.options.retained_results.max(1) {
            self.retained.pop_front();
        }
    }

    pub(crate) fn revalidate_retained(&mut self, app: &Tempo, id: &str) {
        if let Some(entry) = self
            .retained
            .iter_mut()
            .find(|entry| entry.request.request_id == id)
        {
            entry.validate_source(app);
        }
    }
}

impl Drop for GameDetection {
    fn drop(&mut self) {
        if let Some(active) = &self.active {
            active.stop.cancelled.store(true, Ordering::Relaxed);
        }
    }
}

fn addressed_error(
    status: u16,
    code: &str,
    request_id: &str,
    dataset_id: &DatasetId,
) -> DetectionReply {
    DetectionReply {
        status,
        body: json!({
            "schema_version": VERSION,
            "request_id": request_id,
            "dataset_id": dataset_id,
            "status": "error",
            "code": code,
        }),
    }
}

fn descriptor_json(descriptor: &DetectorDescriptor) -> Value {
    json!({
        "id": descriptor.id,
        "revision": descriptor.revision,
        "configuration_fingerprint": descriptor.configuration_fingerprint,
        "label": descriptor.label,
    })
}

fn diagnostic_status(status: &DetectionStatus) -> &'static str {
    match status {
        DetectionStatus::Running => "running",
        DetectionStatus::Complete { .. } => "complete",
        DetectionStatus::Cancelled => "cancelled",
        DetectionStatus::TimedOut => "timed_out",
        DetectionStatus::Unsupported => "unsupported",
        DetectionStatus::InvalidResult => "invalid_result",
        DetectionStatus::Failed => "failed",
        DetectionStatus::SourceUnavailable => "source_unavailable",
    }
}

fn stopped(stop: DetectionStop) -> DetectionStatus {
    match stop {
        DetectionStop::Cancelled => DetectionStatus::Cancelled,
        DetectionStop::Deadline => DetectionStatus::TimedOut,
    }
}

fn detect(task: &Work) -> DetectionStatus {
    let _diagnostic = crate::diagnostics::Span::new("detection.execute");
    if let Err(stop) = task.stop.check() {
        return stopped(stop);
    }
    let Ok(mut games) = parse_played_games(&task.source.pgn) else {
        return DetectionStatus::SourceUnavailable;
    };
    if games.len() != 1 {
        return DetectionStatus::SourceUnavailable;
    }
    let Some(Ok(game)) = games.pop() else {
        return DetectionStatus::SourceUnavailable;
    };
    let input = GameDetectionInput::new(game.timeline(), task.source.color);
    if let Err(stop) = task.stop.check() {
        return stopped(stop);
    }
    let outcome = task.detector.detector.detect(&input, task.stop.as_ref());
    if let Err(stop) = task.stop.check() {
        return stopped(stop);
    }
    let claims = match outcome {
        DetectionOutcome::Complete(claims) => claims,
        DetectionOutcome::Unsupported => return DetectionStatus::Unsupported,
        DetectionOutcome::Stopped(stop) => return stopped(stop),
        DetectionOutcome::Failed(_) => return DetectionStatus::Failed,
    };
    let Ok(moments) = validate_detected_moments(&input, claims) else {
        return DetectionStatus::InvalidResult;
    };
    let input_key = input.fingerprint();
    let descriptor = &task.detector.descriptor;
    let analysis_key = digest(&json!([
        VERSION,
        task.request.dataset_id,
        task.request.source,
        task.request.game_id,
        input_key,
        descriptor.id,
        descriptor.revision,
        descriptor.configuration_fingerprint,
    ]));
    let findings = moments
        .into_iter()
        .map(|moment| DetectionFinding {
            finding_id: digest(&json!([analysis_key, moment.semantic_fingerprint()])),
            moment,
        })
        .collect();
    if let Err(stop) = task.stop.check() {
        return stopped(stop);
    }
    DetectionStatus::Complete {
        learner_color: input.learner(),
        occurrence_count: input.timeline().occurrences().len(),
        input_key,
        analysis_key,
        findings,
    }
}

fn digest(value: &Value) -> String {
    blake3::hash(value.to_string().as_bytes())
        .to_hex()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JsonSnapshotCodec, game_evidence::GameImport, local_runtime};

    struct BlockingDetector {
        entered: SyncSender<std::thread::ThreadId>,
        release: std::sync::Mutex<Receiver<()>>,
    }

    impl GameDetector for BlockingDetector {
        fn descriptor(&self) -> DetectorDescriptor {
            DetectorDescriptor {
                id: "barrier".into(),
                revision: "1".into(),
                configuration_fingerprint: "fixture".into(),
                label: "Barrier fixture".into(),
            }
        }

        fn detect(
            &self,
            _input: &GameDetectionInput,
            _control: &dyn DetectionControl,
        ) -> DetectionOutcome {
            self.entered.send(std::thread::current().id()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            DetectionOutcome::Complete(Vec::new())
        }
    }

    fn report(app: &Tempo, game: &DetectionRequest, jobs: &mut GameDetection) -> Value {
        let started = Instant::now();
        loop {
            let reply = jobs.read(
                app,
                DetectionRead {
                    schema_version: VERSION,
                    request_id: game.request_id.clone(),
                    dataset_id: game.dataset_id.clone(),
                },
            );
            if reply.status != 202 {
                return reply.body;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "worker did not deliver"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn wait_for_acknowledgement(app: &Tempo, jobs: &mut GameDetection) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while jobs.active.is_some() {
            jobs.poll(app);
            assert!(Instant::now() < deadline, "worker did not acknowledge");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn disposable_app() -> (tempfile::TempDir, Tempo) {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("detector.sqlite");
        local_runtime::init(&database).unwrap();
        let app = local_runtime::open(&database).unwrap();
        (directory, app)
    }

    fn seed_game(app: &mut Tempo, id: &str) -> DetectionRequest {
        let original_dataset = app.game_dataset().unwrap();
        app.import_game_pgn(GameImport {
            dataset_id: original_dataset.clone(),
            command_id: "seed".to_owned(),
            learner: "Learner".to_owned(),
            repertoire_id: None,
            pgn: "[White \"Learner\"]\n[Black \"Opponent\"]\n[Result \"1-0\"]\n1.e4 e5 1-0\n"
                .into(),
        })
        .unwrap();
        let history = app.game_history().unwrap();
        let game = &history["results"][0];
        DetectionRequest {
            schema_version: VERSION,
            request_id: id.into(),
            dataset_id: original_dataset,
            source: game["source"].as_str().unwrap().into(),
            game_id: game["game_id"].as_str().unwrap().into(),
            learner: "Learner".into(),
            detector_id: "barrier".into(),
        }
    }

    fn barrier_detector() -> (
        DetectorRegistry,
        Receiver<std::thread::ThreadId>,
        SyncSender<()>,
    ) {
        let (entered, observe) = mpsc::sync_channel(1);
        let (release, wait) = mpsc::sync_channel(1);
        let mut registry = DetectorRegistry::default();
        registry
            .register(BlockingDetector {
                entered,
                release: std::sync::Mutex::new(wait),
            })
            .unwrap();
        (registry, observe, release)
    }

    #[test]
    fn timeout_keeps_worker_occupied_until_acknowledgement() {
        let (_directory, mut app) = disposable_app();
        let a = seed_game(&mut app, "a");
        let (registry, observe, release) = barrier_detector();
        let mut jobs = GameDetection::new(
            registry,
            AnalysisOptions {
                deadline: Duration::from_secs(2),
                ..AnalysisOptions::default()
            },
        )
        .unwrap();

        assert_eq!(jobs.start(&app, a.clone()).status, 202);
        let worker = observe.recv_timeout(Duration::from_secs(2)).unwrap();
        let timeout = report(&app, &a, &mut jobs);
        assert_eq!(timeout["status"], "timed_out");
        for id in ["busy-1", "busy-2", "busy-3"] {
            let reply = jobs.start(
                &app,
                DetectionRequest {
                    request_id: id.into(),
                    ..a.clone()
                },
            );
            assert_eq!(reply.status, 409);
            assert_eq!(reply.body["code"], "busy");
            assert_eq!(reply.body["request_id"], id);
        }
        assert!(matches!(observe.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(jobs.start(&app, a.clone()).body, timeout);
        assert!(jobs.active.is_some());

        release.send(()).unwrap();
        wait_for_acknowledgement(&app, &mut jobs);
        assert_eq!(jobs.start(&app, a.clone()).body, timeout);
        let b = DetectionRequest {
            request_id: "b".into(),
            ..a
        };
        assert_eq!(jobs.start(&app, b.clone()).status, 202);
        assert_eq!(
            observe.recv_timeout(Duration::from_secs(2)).unwrap(),
            worker
        );
        release.send(()).unwrap();
        assert_eq!(report(&app, &b, &mut jobs)["status"], "complete");
        assert!(matches!(observe.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn late_success_does_not_replace_the_addressed_timeout() {
        let (_directory, mut app) = disposable_app();
        let a = seed_game(&mut app, "a");
        let (registry, _observe, _release) = barrier_detector();
        let (work, incoming) = mpsc::sync_channel(1);
        let mut jobs = GameDetection {
            registry,
            options: AnalysisOptions {
                deadline: Duration::ZERO,
                ..AnalysisOptions::default()
            },
            work,
            active: None,
            retained: VecDeque::new(),
        };

        assert_eq!(jobs.start(&app, a.clone()).status, 202);
        let task = incoming.try_recv().unwrap();
        let timeout = report(&app, &a, &mut jobs);
        assert_eq!(timeout["status"], "timed_out");
        // Model delivery after the deadline of a success computed before it.
        task.reply
            .send(DetectionStatus::Complete {
                learner_color: task.source.color,
                occurrence_count: 3,
                input_key: "fixture-input".into(),
                analysis_key: "fixture-analysis".into(),
                findings: Vec::new(),
            })
            .unwrap();
        wait_for_acknowledgement(&app, &mut jobs);
        assert_eq!(jobs.start(&app, a).body, timeout);
    }

    #[test]
    fn replay_keeps_the_worker_slot_and_dataset_replacement_fences_delivery() {
        let (_directory, mut app) = disposable_app();
        let empty = app.export(&JsonSnapshotCodec).unwrap();
        let a = seed_game(&mut app, "a");
        let b = DetectionRequest {
            request_id: "b".into(),
            ..a.clone()
        };
        let c = DetectionRequest {
            request_id: "c".into(),
            ..a.clone()
        };
        let (registry, observe, release) = barrier_detector();
        let mut jobs = GameDetection::new(registry, AnalysisOptions::default()).unwrap();

        assert_eq!(jobs.start(&app, a.clone()).status, 202);
        observe.recv_timeout(Duration::from_secs(2)).unwrap();
        release.send(()).unwrap();
        let a_reply = report(&app, &a, &mut jobs);
        assert_eq!(a_reply["status"], "complete");
        assert_eq!(jobs.start(&app, b.clone()).status, 202);
        observe.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(jobs.start(&app, a.clone()).body, a_reply);
        assert_eq!(jobs.start(&app, c).body["code"], "busy");

        app.restore("replacement".into(), &empty, &JsonSnapshotCodec)
            .unwrap();
        let stale = report(&app, &b, &mut jobs);
        assert_eq!(stale["status"], "source_unavailable");
        assert!(
            jobs.active
                .as_ref()
                .unwrap()
                .stop
                .cancelled
                .load(Ordering::Relaxed)
        );
        assert_eq!(jobs.start(&app, a).body["status"], "source_unavailable");
        release.send(()).unwrap();
        wait_for_acknowledgement(&app, &mut jobs);
        assert_eq!(report(&app, &b, &mut jobs), stale);
        let current = seed_game(&mut app, "current");
        assert_ne!(current.dataset_id, b.dataset_id);
        assert_eq!(jobs.start(&app, current.clone()).status, 202);
        observe.recv_timeout(Duration::from_secs(2)).unwrap();
        release.send(()).unwrap();
        assert_eq!(report(&app, &current, &mut jobs)["status"], "complete");
    }
}
