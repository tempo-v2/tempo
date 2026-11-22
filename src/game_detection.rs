//! Ephemeral, addressed execution of stored-game analysis.
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
    DetectionControl, DetectionOutcome, DetectionStop, DetectorDescriptor, DetectorIdentity,
    GameDetectionInput, GameDetector, LearnerColor, PositionFinding, ValidatedMoment,
    parse_played_games, validate_detected_moments,
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
    tactical_format: Option<tempo_chess::TacticalEvidenceFormat>,
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
        let tactical_format = detector.tactical_evidence_format();
        if tactical_format == Some(tempo_chess::TacticalEvidenceFormat::CompleteDecision)
            && self
                .0
                .values()
                .any(|entry| entry.tactical_format == tactical_format)
        {
            return Err(AppError::new(
                "duplicate_complete_decision_detector",
                "Only one detector may own the complete game-decision evidence.",
            ));
        }
        self.0.insert(
            descriptor.id.clone(),
            RegisteredDetector {
                descriptor,
                tactical_format,
                detector: Arc::new(detector),
            },
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DetectionRequest {
    pub(crate) schema_version: u8,
    pub(crate) request_id: String,
    pub(crate) dataset_id: DatasetId,
    pub(crate) source: String,
    pub(crate) game_id: String,
    pub(crate) learner: String,
    pub(crate) detector_id: String,
}

#[cfg(test)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DetectionRead {
    schema_version: u8,
    request_id: String,
    dataset_id: DatasetId,
}

#[derive(Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum DetectionStatus {
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

#[derive(Clone, Serialize)]
pub(crate) struct DetectionFinding {
    #[serde(skip)]
    pub(crate) producer: DetectorIdentity,
    #[serde(skip)]
    pub(crate) skipped: Vec<DetectorIdentity>,
    finding_id: String,
    #[serde(flatten)]
    pub(crate) moment: ValidatedMoment,
}

pub(crate) struct DetectionReply {
    /// True only when this call queued new work, including when the wire
    /// status of a replay is also 202.
    pub(crate) started: bool,
    pub(crate) status: u16,
    #[cfg(test)]
    pub(crate) body: Value,
}

/// A typed, source-validated completion for an explicitly composed application
/// consumer. Reading or previewing a detector result still has no side effects.
pub(crate) struct DetectionCompletion {
    pub(crate) analyses: Vec<DetectorIdentity>,
    pub(crate) position: Option<tempo_store::TacticalPositionRun>,
    pub(crate) request: DetectionRequest,
    pub(crate) source: AcceptedDetectionSource,
    pub(crate) status: DetectionStatus,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AcceptedDetectionSource {
    pub(crate) pgn: String,
    pub(crate) color: LearnerColor,
}

struct Stop {
    deeper: bool,
    deadline: Instant,
    cancelled: AtomicBool,
}

impl DetectionControl for Stop {
    fn deeper_search(&self) -> bool {
        self.deeper
    }
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
    position_detectors: Vec<RegisteredDetector>,
    position: Option<tempo_store::TacticalPositionRun>,
    diagnostic_context: Option<String>,
    request: DetectionRequest,
    source: Arc<AcceptedDetectionSource>,
    detector: RegisteredDetector,
    stop: Arc<Stop>,
    reply: SyncSender<DetectionStatus>,
}

struct Entry {
    analyses: Vec<DetectorIdentity>,
    position: Option<tempo_store::TacticalPositionRun>,
    request: DetectionRequest,
    source: Arc<AcceptedDetectionSource>,
    #[cfg(test)]
    descriptor: DetectorDescriptor,
    status: DetectionStatus,
}

impl Entry {
    fn reply(&self) -> DetectionReply {
        #[cfg(test)]
        let body = {
            let mut body = json!({
                "schema_version": VERSION,
                "request_id": self.request.request_id,
                "dataset_id": self.request.dataset_id,
                "source": self.request.source,
                "game_id": self.request.game_id,
                "detector": descriptor_json(&self.descriptor),
            });
            let mut status =
                serde_json::to_value(&self.status).expect("serializable detection status");
            // Wire 1 is a display-only preview, consumed by older strict clients.
            // Typed training evidence is accepted through the application command
            // and exposed by its separate versioned history read.
            if let Some(findings) = status.get_mut("findings").and_then(Value::as_array_mut) {
                for finding in findings {
                    if let Some(fields) = finding.as_object_mut() {
                        fields.remove("tactical");
                    }
                }
            }
            body.as_object_mut()
                .unwrap()
                .extend(status.as_object().unwrap().clone());
            body
        };
        DetectionReply {
            started: false,
            status: if matches!(self.status, DetectionStatus::Running) {
                202
            } else {
                200
            },
            #[cfg(test)]
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
    #[cfg(test)]
    pub(crate) fn descriptor(&self, id: &str) -> Option<DetectorDescriptor> {
        self.registry
            .0
            .get(id)
            .map(|entry| entry.descriptor.clone())
    }

    pub(crate) fn tactical_registration(
        &self,
        id: &str,
    ) -> Option<(DetectorDescriptor, tempo_chess::TacticalEvidenceFormat)> {
        let entry = self.registry.0.get(id)?;
        Some((entry.descriptor.clone(), entry.tactical_format?))
    }

    pub(crate) fn complete_decision_detector(&self) -> Option<&str> {
        self.registry.0.iter().find_map(|(id, entry)| {
            (entry.tactical_format == Some(tempo_chess::TacticalEvidenceFormat::CompleteDecision))
                .then_some(id.as_str())
        })
    }

    pub(crate) fn idle(&self) -> bool {
        self.active.is_none()
    }

    pub(crate) fn completion(&mut self, app: &Tempo, id: &str) -> Option<DetectionCompletion> {
        self.poll(app);
        self.revalidate_retained(app, id);
        let entry = self.lookup(id)?;
        if matches!(entry.status, DetectionStatus::Running) {
            return None;
        }
        Some(DetectionCompletion {
            analyses: entry.analyses.clone(),
            position: entry.position,
            request: entry.request.clone(),
            source: (*entry.source).clone(),
            status: entry.status.clone(),
        })
    }

    pub(crate) fn new(
        registry: DetectorRegistry,
        options: AnalysisOptions,
    ) -> std::io::Result<Self> {
        let _diagnostic = crate::diagnostics::Span::new("detection.initialize");
        let (work, incoming) = mpsc::sync_channel::<Work>(1);
        std::thread::Builder::new()
            .name("tempo-game-detection".into())
            .spawn(move || {
                let mut cached = None;
                while let Ok(task) = incoming.recv() {
                    crate::diagnostics::with_context(task.diagnostic_context.clone(), || {
                        let mut trace = crate::diagnostics::Span::new("detection.worker");
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            detect(&task, &mut cached)
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

    #[cfg(test)]
    pub(crate) fn start(&mut self, app: &Tempo, request: DetectionRequest) -> DetectionReply {
        self.start_work(app, request, None, &[])
    }

    pub(crate) fn cancel_background(&mut self) {
        if let Some(active) = &self.active
            && active.entry.position.is_some()
        {
            active.stop.cancelled.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn start_work(
        &mut self,
        app: &Tempo,
        request: DetectionRequest,
        position: Option<tempo_store::TacticalPositionRun>,
        analyses: &[DetectorIdentity],
    ) -> DetectionReply {
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
        let mut analyses = analyses.to_vec();
        analyses.sort();
        if analyses.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return error(400, "duplicate_position_detector");
        }
        self.poll(app);
        self.revalidate_retained(app, &request.request_id);
        if let Some(entry) = self.lookup(&request.request_id) {
            return if entry.request == request
                && entry.position == position
                && entry.analyses == analyses
            {
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
        let Some(position_detectors) = analyses
            .iter()
            .map(|member| {
                self.registry
                    .0
                    .get(&member.id)
                    .filter(|entry| entry.descriptor.identity() == *member)
                    .cloned()
            })
            .collect::<Option<Vec<_>>>()
        else {
            return error(404, "unknown_position_detector");
        };
        if position.is_some() && position_detectors.is_empty() {
            return error(400, "empty_analysis_set");
        }
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
            deeper: position.is_some_and(|run| run.deeper),
            deadline,
            cancelled: AtomicBool::new(false),
        });
        let (reply, result) = mpsc::sync_channel(1);
        let entry = Entry {
            analyses,
            position,
            request: request.clone(),
            source: source.clone(),
            #[cfg(test)]
            descriptor: detector.descriptor.clone(),
            status: DetectionStatus::Running,
        };
        let diagnostic_context = crate::diagnostics::capture_context();
        if self
            .work
            .try_send(Work {
                position,
                position_detectors,
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
        let mut delivery = entry.reply();
        delivery.started = true;
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

    #[cfg(test)]
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
    _code: &str,
    _request_id: &str,
    _dataset_id: &DatasetId,
) -> DetectionReply {
    DetectionReply {
        started: false,
        status,
        #[cfg(test)]
        body: json!({
            "schema_version": VERSION,
            "request_id": _request_id,
            "dataset_id": _dataset_id,
            "status": "error",
            "code": _code,
        }),
    }
}

#[cfg(test)]
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

type CachedDetection = (Arc<AcceptedDetectionSource>, GameDetectionInput, String);

fn detect(task: &Work, cached: &mut Option<CachedDetection>) -> DetectionStatus {
    let _diagnostic = crate::diagnostics::Span::new("detection.execute");
    if let Err(stop) = task.stop.check() {
        return stopped(stop);
    }
    if !cached
        .as_ref()
        .is_some_and(|(source, _, _)| **source == *task.source)
    {
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
        let key = input.fingerprint();
        *cached = Some((task.source.clone(), input, key));
    }
    let (_, input, input_key) = cached.as_ref().expect("cached source");
    if let Err(stop) = task.stop.check() {
        return stopped(stop);
    }
    let detector = &task.detector.detector;
    let outcome = if let Some(run) = task.position {
        tempo_chess::scan_position(
            input,
            run.ply as usize,
            task.position_detectors
                .iter()
                .map(|entry| entry.detector.as_ref()),
            task.stop.as_ref(),
        )
    } else {
        match detector.detect(input, task.stop.as_ref()) {
            DetectionOutcome::Complete(moments) => DetectionOutcome::Complete(
                moments
                    .into_iter()
                    .map(|moment| PositionFinding {
                        producer: task.detector.descriptor.identity(),
                        moment,
                        skipped: vec![],
                    })
                    .collect(),
            ),
            DetectionOutcome::Unsupported => DetectionOutcome::Unsupported,
            DetectionOutcome::Stopped(reason) => DetectionOutcome::Stopped(reason),
            DetectionOutcome::Failed(error) => DetectionOutcome::Failed(error),
        }
    };
    if let Err(stop) = task.stop.check() {
        return stopped(stop);
    }
    let claims = match outcome {
        DetectionOutcome::Complete(claims) => claims,
        DetectionOutcome::Unsupported => return DetectionStatus::Unsupported,
        DetectionOutcome::Stopped(stop) => return stopped(stop),
        DetectionOutcome::Failed(_) => return DetectionStatus::Failed,
    };
    let descriptor = &task.detector.descriptor;
    let analysis_key = if task.position.is_none() {
        digest(&json!([
            VERSION,
            task.request.dataset_id,
            task.request.source,
            task.request.game_id,
            input_key,
            descriptor.id,
            descriptor.revision,
            descriptor.configuration_fingerprint
        ]))
    } else {
        digest(&json!([
            VERSION,
            task.request.dataset_id,
            task.request.source,
            task.request.game_id,
            input_key,
            task.position_detectors
                .iter()
                .map(|member| member.descriptor.identity())
                .collect::<Vec<_>>()
        ]))
    };
    let mut by_producer = BTreeMap::<DetectorIdentity, Vec<_>>::new();
    let mut skipped = vec![];
    for finding in claims {
        let producer = if task.position.is_some() {
            task.position_detectors
                .iter()
                .find(|entry| entry.descriptor.identity() == finding.producer)
        } else {
            Some(&task.detector)
        };
        if finding.moment.tactical.as_ref().is_some_and(|assessment| {
            let required = match assessment.kind {
                tempo_chess::TacticalKind::PersonalTactics => {
                    tempo_chess::TacticalEvidenceFormat::CompleteDecision
                }
                _ => tempo_chess::TacticalEvidenceFormat::LegacyFindings,
            };
            producer.and_then(|entry| entry.tactical_format) != Some(required)
        }) {
            return DetectionStatus::InvalidResult;
        }
        if task
            .position
            .is_some_and(|run| finding.moment.occurrence != run.ply as usize)
        {
            return DetectionStatus::InvalidResult;
        }
        skipped.extend(finding.skipped);
        by_producer
            .entry(finding.producer)
            .or_default()
            .push(finding.moment);
    }
    let mut findings = Vec::new();
    for (producer, claims) in by_producer {
        let Ok(moments) = validate_detected_moments(input, claims) else {
            return DetectionStatus::InvalidResult;
        };
        for moment in moments {
            let key = if task.position.is_none() {
                json!([analysis_key, moment.semantic_fingerprint()])
            } else {
                json!([analysis_key, producer, moment.semantic_fingerprint()])
            };
            findings.push(DetectionFinding {
                finding_id: digest(&key),
                producer: producer.clone(),
                skipped: skipped.clone(),
                moment,
            });
        }
    }
    if let Err(stop) = task.stop.check() {
        return stopped(stop);
    }
    DetectionStatus::Complete {
        learner_color: input.learner(),
        occurrence_count: input.timeline().occurrences().len(),
        input_key: input_key.clone(),
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
        seed_line(app, id, "1.e4 e5 1-0")
    }

    fn seed_line(app: &mut Tempo, id: &str, line: &str) -> DetectionRequest {
        let original_dataset = app.game_dataset().unwrap();
        app.import_game_pgn(GameImport {
            dataset_id: original_dataset.clone(),
            command_id: "seed".to_owned(),
            learner: "Learner".to_owned(),
            repertoire_id: None,
            pgn: format!("[White \"Learner\"]\n[Black \"Opponent\"]\n[Result \"1-0\"]\n{line}\n"),
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

    #[test]
    fn queued_detector_set_reuses_one_search_inside_the_real_host() {
        use std::sync::atomic::AtomicUsize;
        use tempo_chess::{
            EvaluationFailure, EvaluationProvenance, EvaluationStatus, PositionPass,
            RootEvaluation, TacticalEvaluationRequest, TacticalEvaluator,
        };

        struct Engine(Arc<AtomicUsize>);
        impl TacticalEvaluator for Engine {
            fn descriptor(&self) -> EvaluationProvenance {
                EvaluationProvenance {
                    engine: "fixture".into(),
                    engine_version: "1".into(),
                    configuration_fingerprint: "shared".into(),
                }
            }
            fn evaluate(
                &self,
                request: &TacticalEvaluationRequest,
                _: &dyn DetectionControl,
            ) -> Result<RootEvaluation, EvaluationFailure> {
                self.0.fetch_add(1, Ordering::SeqCst);
                assert!(request.legal_moves.contains(&"g1f3".into()));
                assert_eq!(request.moves, ["e2e4", "e7e5"]);
                assert_ne!(request.fen, request.start_fen);
                Err(EvaluationFailure::Incomplete)
            }
        }

        struct Probe(&'static str, Arc<AtomicUsize>, Arc<AtomicUsize>);
        impl GameDetector for Probe {
            fn descriptor(&self) -> DetectorDescriptor {
                DetectorDescriptor {
                    id: self.0.into(),
                    label: self.0.into(),
                    revision: "1".into(),
                    configuration_fingerprint: "fixture".into(),
                }
            }

            fn detect(&self, _: &GameDetectionInput, _: &dyn DetectionControl) -> DetectionOutcome {
                panic!("a position task called the whole-game detector")
            }

            fn detect_position(
                &self,
                position: &PositionPass<'_>,
                control: &dyn DetectionControl,
            ) -> DetectionOutcome {
                assert_eq!(position.occurrence().next_move().unwrap().uci, "g1f3");
                assert_eq!(
                    position
                        .played_prefix()
                        .map(|answer| answer.uci.as_str())
                        .collect::<Vec<_>>(),
                    ["e2e4", "e7e5"]
                );
                self.1.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    position.piece_at("e4").unwrap().role,
                    tempo_chess::PieceRole::Pawn
                );
                assert!(
                    position
                        .legal_moves()
                        .iter()
                        .any(|answer| answer.uci == "g1f3")
                );
                assert_eq!(
                    position.compare(&Engine(self.2.clone()), control).unwrap(),
                    EvaluationStatus::Incomplete { evaluation: None }
                );
                DetectionOutcome::Complete(vec![tempo_chess::DetectedMoment {
                    occurrence: position.occurrence().offset(),
                    code: "same_motif".into(),
                    subjects: vec![],
                    candidate_move: Some("g1f3".into()),
                    explanation: self.0.into(),
                    tactical: None,
                }])
            }
        }

        let (_directory, mut app) = disposable_app();
        let request = seed_line(&mut app, "shared", "1.e4 e5 2.Nf3 1-0");
        let probes = Arc::new(AtomicUsize::new(0));
        let searches = Arc::new(AtomicUsize::new(0));
        let mut registry = DetectorRegistry::default();
        for id in ["barrier", "second"] {
            registry
                .register(Probe(id, probes.clone(), searches.clone()))
                .unwrap();
        }
        let mut jobs = GameDetection::new(registry, AnalysisOptions::default()).unwrap();
        let members = ["barrier", "second"].map(|id| jobs.descriptor(id).unwrap().identity());
        let mut obsolete = members.clone();
        obsolete[1].revision = "0".into();
        let run = Some(tempo_store::TacticalPositionRun {
            id: 1,
            ply: 2,
            deeper: false,
        });
        assert_eq!(
            jobs.start_work(&app, request.clone(), run, &obsolete)
                .status,
            404
        );
        assert_ne!(
            crate::application::tactical_analysis::analysis_version(&members),
            crate::application::tactical_analysis::analysis_version(&obsolete),
        );
        let delivery = jobs.start_work(&app, request.clone(), run, &members);
        assert_eq!(delivery.status, 202);
        let reply = report(&app, &request, &mut jobs);
        assert_eq!(reply["status"], "complete");
        let DetectionStatus::Complete { findings, .. } =
            jobs.completion(&app, &request.request_id).unwrap().status
        else {
            panic!("expected a completed position")
        };
        assert_eq!(
            findings
                .iter()
                .map(|finding| finding.producer.clone())
                .collect::<Vec<_>>(),
            members
        );
        assert_ne!(
            reply["findings"][0]["finding_id"],
            reply["findings"][1]["finding_id"]
        );
        assert_eq!(probes.load(Ordering::SeqCst), 2);
        assert_eq!(searches.load(Ordering::SeqCst), 1);
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

        assert!(jobs.start(&app, a.clone()).started);
        let running_replay = jobs.start(&app, a.clone());
        assert_eq!(running_replay.status, 202);
        assert!(!running_replay.started);
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
