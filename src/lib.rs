//! Application composition: domain decisions in, atomic persistence and views out.
pub mod wire;

use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};
use tempo_chess::{
    Catalog, ChangeKind, ChessAnswer, ChessDefinition, ChessFeedback, ChessPrompt, ChessState,
    ContentParser, ExactSequenceGrader, GameComparator, ImportRequest,
};
use tempo_learning::{
    AssessmentInput, AssessmentSource, FixedIntervalScheduler, LearningPolicy, LearningState,
    SchedulerSettings, StandardLearning, Transition,
};
use tempo_queue::{
    Candidate, Group, Line, OrderEvent, QueueInput, QueueOrderContext, QueuePolicy, StableQueue,
};
use tempo_store::{
    AnswerResult, AssessmentKey, AssessmentRecord, Commit, MemoryStore, Receipt, ReceiptResult,
    Snapshot, Store, StoreError, StoredAttempt, Write,
};
use tempo_study::{AttemptStatus, Availability, Exercise, Grader, QuestionSource};
use tempo_types::{
    AttemptId, ContentVersion, DatasetId, EventId, Grade, Revision, StudyDay, StudyTime,
    SubmissionId, TargetId,
};

pub type ChessGrader = dyn Grader<
        Definition = ChessDefinition,
        State = ChessState,
        Answer = ChessAnswer,
        Prompt = ChessPrompt,
        Feedback = ChessFeedback,
    >;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}
impl AppError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
        }
    }
}
impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl Error for AppError {}
impl From<StoreError> for AppError {
    fn from(value: StoreError) -> Self {
        let code = match &value {
            StoreError::Unavailable(_) => "store_unavailable",
            StoreError::Conflict(_) => "store_conflict",
            StoreError::Invalid(_) => "store_invalid",
        };
        Self {
            code: code.into(),
            message: value.to_string(),
            retryable: !matches!(value, StoreError::Invalid(_)),
        }
    }
}
fn domain(code: &str, error: impl fmt::Display) -> AppError {
    AppError::new(code, error.to_string())
}

pub trait Clock {
    fn now(&self) -> StudyTime;
}
pub struct FixedClock(pub StudyTime);
impl Clock for FixedClock {
    fn now(&self) -> StudyTime {
        self.0
    }
}
pub trait AttemptIds {
    fn next_id(&mut self) -> AttemptId;
}
#[derive(Default)]
pub struct CounterIds(pub u64);
impl AttemptIds for CounterIds {
    fn next_id(&mut self) -> AttemptId {
        self.0 += 1;
        AttemptId(format!("demo-attempt-{}", self.0))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameRange {
    pub from_ms: u64,
    pub to_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Game {
    pub provider: String,
    pub id: String,
    pub observed_at_ms: u64,
    pub pgn: String,
}
pub trait GameProvider {
    fn fetch(&self, range: &GameRange) -> Result<Vec<Game>, AppError>;
}
pub struct FixtureGames(pub Vec<Game>);
impl GameProvider for FixtureGames {
    fn fetch(&self, range: &GameRange) -> Result<Vec<Game>, AppError> {
        if range.from_ms > range.to_ms {
            return Err(AppError::new("invalid_range", "start follows end"));
        }
        Ok(self
            .0
            .iter()
            .filter(|g| g.observed_at_ms >= range.from_ms && g.observed_at_ms <= range.to_ms)
            .cloned()
            .collect())
    }
}
pub struct UnavailableGames;
impl GameProvider for UnavailableGames {
    fn fetch(&self, _: &GameRange) -> Result<Vec<Game>, AppError> {
        Err(AppError::new(
            "provider_unavailable",
            "fixture provider is unavailable",
        ))
    }
}

/// Only this composition object knows all concrete dependencies.
pub struct Components {
    pub queue: Box<dyn QueuePolicy>,
    pub learning: Box<dyn LearningPolicy>,
    pub grader: Box<ChessGrader>,
    pub parser: Box<dyn ContentParser>,
    pub comparator: Box<dyn GameComparator>,
    pub clock: Box<dyn Clock>,
    pub ids: Box<dyn AttemptIds>,
}
impl Components {
    pub fn fixtures() -> Self {
        Self {
            queue: Box::new(StableQueue),
            learning: Box::new(StandardLearning {
                scheduler: FixedIntervalScheduler::default(),
            }),
            grader: Box::new(ExactSequenceGrader),
            parser: Box::new(tempo_chess::demo_parser()),
            comparator: Box::new(tempo_chess::demo_comparator()),
            clock: Box::new(FixedClock(StudyTime {
                day: StudyDay(0),
                unix_ms: 10,
            })),
            ids: Box::new(CounterIds::default()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitCommand {
    pub dataset_id: DatasetId,
    pub attempt_id: AttemptId,
    pub submission_id: SubmissionId,
    pub expected_revision: Revision,
    pub answer: ChessAnswer,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbandonCommand {
    pub dataset_id: DatasetId,
    pub attempt_id: AttemptId,
    pub submission_id: SubmissionId,
    pub expected_revision: Revision,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgainCommand {
    pub dataset_id: DatasetId,
    pub target: TargetId,
    pub expected_content_version: ContentVersion,
    pub provider: String,
    pub event_id: EventId,
    pub observed_at_ms: u64,
}

pub struct Tempo {
    store: Box<dyn Store>,
    components: Components,
    remaining_block: Vec<TargetId>,
    order_history: Vec<OrderEvent>,
}
impl Tempo {
    pub fn new(store: Box<dyn Store>, components: Components) -> Result<Self, AppError> {
        let snapshot = store.snapshot()?;
        if snapshot.policy != components.learning.settings() {
            return Err(AppError::new(
                "policy_mismatch",
                "stored learning policy differs from selected implementation",
            ));
        }
        for state in snapshot.learning.values() {
            components
                .learning
                .validate_state(state)
                .map_err(|e| domain("invalid_learning", e))?;
        }
        Ok(Self {
            store,
            components,
            remaining_block: vec![],
            order_history: vec![],
        })
    }
    pub fn demo() -> Result<Self, AppError> {
        let components = Components::fixtures();
        let store = MemoryStore::new(
            "demo-dataset".into(),
            tempo_chess::demo_catalog(),
            components.learning.settings(),
        )?;
        Self::new(Box::new(store), components)
    }
    pub fn snapshot(&self) -> Result<Snapshot, AppError> {
        self.store.snapshot().map_err(Into::into)
    }
    fn commit(&mut self, snapshot: &Snapshot, writes: Vec<Write>) -> Result<(), AppError> {
        self.store.commit(Commit {
            expected_revision: snapshot.revision,
            expected_dataset: snapshot.dataset.clone(),
            writes,
        })?;
        Ok(())
    }
    pub fn next_exercise(&mut self) -> Result<Option<AnswerResult>, AppError> {
        let snapshot = self.store.snapshot()?;
        if let Some(id) = &snapshot.active_attempt {
            return self.resume_from(&snapshot, id).map(Some);
        }
        let context = QueueOrderContext {
            seed: snapshot.revision.0,
            history: self.order_history.clone(),
        };
        let mut source = QueueSource {
            snapshot: &snapshot,
            queue: self.components.queue.as_ref(),
            grader: self.components.grader.as_ref(),
            today: self.components.clock.now().day,
            context,
            remaining: self.remaining_block.clone(),
        };
        let id = self.components.ids.next_id();
        let result = tempo_study::next(
            &mut source,
            self.components.grader.as_ref(),
            snapshot.dataset.clone(),
            id,
        )
        .map_err(|e| domain("source_or_grader_error", e))?;
        let remaining = source.remaining;
        let Availability::Ready(started) = result else {
            return Ok(None);
        };
        let state = snapshot
            .learning
            .get(&started.attempt.target)
            .ok_or_else(|| {
                AppError::new("invalid_learning", "selected target has no learning state")
            })?;
        let row = StoredAttempt {
            attempt: started.attempt,
            issued_at: self.components.clock.now(),
            issued_miss_revision: state.miss_revision,
        };
        self.commit(
            &snapshot,
            vec![
                Write::Issue(row.clone()),
                Write::Active(Some(row.attempt.id.clone())),
            ],
        )?;
        self.remaining_block = remaining;
        Ok(Some(AnswerResult {
            attempt: row.attempt,
            feedback: None,
            assessment: None,
            learning_effect: None,
        }))
    }
    fn current<'a>(
        &self,
        snapshot: &'a Snapshot,
        dataset: &DatasetId,
        id: &AttemptId,
    ) -> Result<&'a StoredAttempt, AppError> {
        if dataset != &snapshot.dataset {
            return Err(AppError::new(
                "stale_dataset",
                "command belongs to a different dataset",
            ));
        }
        let record = snapshot
            .attempts
            .get(id)
            .ok_or_else(|| AppError::new("unknown_attempt", "attempt does not exist"))?;
        if &record.attempt.dataset != dataset {
            return Err(AppError::new(
                "stale_dataset",
                "attempt belongs to a different dataset",
            ));
        }
        let item = snapshot
            .catalog
            .items
            .get(&record.attempt.target)
            .ok_or_else(|| AppError::new("stale_content", "target is unavailable"))?;
        if !item.active() || item.version != record.attempt.content_version {
            return Err(AppError::new(
                "stale_content",
                "exercise changed after this attempt was issued",
            ));
        }
        Ok(record)
    }
    fn resume_from(&self, snapshot: &Snapshot, id: &AttemptId) -> Result<AnswerResult, AppError> {
        let record = self.current(snapshot, &snapshot.dataset, id)?;
        Ok(AnswerResult {
            attempt: record.attempt.clone(),
            feedback: None,
            assessment: None,
            learning_effect: None,
        })
    }
    pub fn resume(&self, id: &AttemptId) -> Result<AnswerResult, AppError> {
        self.resume_from(&self.store.snapshot()?, id)
    }
    pub fn submit(&mut self, command: SubmitCommand) -> Result<AnswerResult, AppError> {
        let snapshot = self.store.snapshot()?;
        let fingerprint =
            serde_json::to_string(&("answer", &command)).map_err(|e| domain("codec_error", e))?;
        if let Some(result) = replay(&snapshot, &command.submission_id, &fingerprint)? {
            return study_result(result);
        }
        let old = self.current(&snapshot, &command.dataset_id, &command.attempt_id)?;
        let definition = &snapshot.catalog.items[&old.attempt.target].definition;
        let progress = tempo_study::submit(
            self.components.grader.as_ref(),
            definition,
            &old.attempt,
            command.expected_revision,
            &command.answer,
        )
        .map_err(|e| domain("invalid_submission", e))?;
        let updated = StoredAttempt {
            attempt: progress.attempt.clone(),
            ..old.clone()
        };
        let mut writes = vec![Write::Progress(updated)];
        let mut effect = None;
        if let Some(grade) = progress.assessment {
            let input = AssessmentInput {
                grade,
                accepted_at: self.components.clock.now(),
                source: AssessmentSource::Human {
                    attempt_id: command.attempt_id.clone(),
                    issued_miss_revision: old.issued_miss_revision,
                },
            };
            let transition = self
                .components
                .learning
                .apply(&snapshot.learning[&old.attempt.target], &input)
                .map_err(|e| domain("learning_error", e))?;
            effect = Some(transition.effect);
            writes.push(Write::Learning {
                target: old.attempt.target.clone(),
                state: transition.state.clone(),
            });
            writes.push(Write::Assessment(AssessmentRecord {
                key: AssessmentKey::Human(command.attempt_id.clone()),
                dataset: snapshot.dataset.clone(),
                target: old.attempt.target.clone(),
                content_version: old.attempt.content_version,
                input,
                transition,
            }));
        }
        let result = AnswerResult {
            attempt: progress.attempt,
            feedback: Some(progress.feedback),
            assessment: progress.assessment,
            learning_effect: effect,
        };
        writes.push(Write::Receipt(Receipt {
            id: command.submission_id,
            command: fingerprint,
            result: ReceiptResult::Study(result.clone()),
        }));
        if result.attempt.status != AttemptStatus::Active
            && snapshot.active_attempt.as_ref() == Some(&result.attempt.id)
        {
            writes.push(Write::Active(None));
        }
        self.commit(&snapshot, writes)?;
        if result.assessment == Some(Grade::Again) {
            self.order_history.push(OrderEvent::Missed {
                target: result.attempt.target.clone(),
            });
        }
        if result.attempt.status == AttemptStatus::Complete {
            self.order_history.push(OrderEvent::Completed {
                target: result.attempt.target.clone(),
            });
        }
        Ok(result)
    }
    pub fn abandon(&mut self, command: AbandonCommand) -> Result<AnswerResult, AppError> {
        let snapshot = self.store.snapshot()?;
        let fingerprint =
            serde_json::to_string(&("abandon", &command)).map_err(|e| domain("codec_error", e))?;
        if let Some(result) = replay(&snapshot, &command.submission_id, &fingerprint)? {
            return study_result(result);
        }
        let old = self.current(&snapshot, &command.dataset_id, &command.attempt_id)?;
        let attempt = tempo_study::abandon(&old.attempt, command.expected_revision)
            .map_err(|e| domain("invalid_submission", e))?;
        let result = AnswerResult {
            attempt: attempt.clone(),
            feedback: None,
            assessment: None,
            learning_effect: None,
        };
        let mut writes = vec![
            Write::Progress(StoredAttempt {
                attempt,
                ..old.clone()
            }),
            Write::Receipt(Receipt {
                id: command.submission_id,
                command: fingerprint,
                result: ReceiptResult::Study(result.clone()),
            }),
        ];
        if snapshot.active_attempt.as_ref() == Some(&command.attempt_id) {
            writes.push(Write::Active(None));
        }
        self.commit(&snapshot, writes)?;
        Ok(result)
    }
    pub fn again(&mut self, command: AgainCommand) -> Result<Transition, AppError> {
        let snapshot = self.store.snapshot()?;
        let fingerprint =
            serde_json::to_string(&("again", &command)).map_err(|e| domain("codec_error", e))?;
        let identity =
            serde_json::to_string(&(&command.dataset_id, &command.provider, &command.event_id))
                .map_err(|e| domain("codec_error", e))?;
        let submission = SubmissionId(format!("external:{identity}"));
        if let Some(result) = replay(&snapshot, &submission, &fingerprint)? {
            return match result {
                ReceiptResult::External { transition, .. } => Ok(transition),
                _ => Err(AppError::new("submission_conflict", "receipt kind differs")),
            };
        }
        if command.dataset_id != snapshot.dataset {
            return Err(AppError::new(
                "stale_dataset",
                "command belongs to a different dataset",
            ));
        }
        if command.provider.is_empty() || command.event_id.0.is_empty() {
            return Err(AppError::new(
                "invalid_event",
                "source and event identity are required",
            ));
        }
        let item = snapshot
            .catalog
            .items
            .get(&command.target)
            .ok_or_else(|| AppError::new("unknown_target", "feedback target does not exist"))?;
        if item.version != command.expected_content_version {
            return Err(AppError::new(
                "stale_content",
                "feedback was computed against a different content version",
            ));
        }
        if !item.active() {
            return Err(AppError::new(
                "inactive_target",
                "feedback target is quarantined",
            ));
        }
        let input = AssessmentInput {
            grade: Grade::Again,
            accepted_at: self.components.clock.now(),
            source: AssessmentSource::External {
                provider: command.provider.clone(),
                event_id: command.event_id.clone(),
                observed_at_ms: command.observed_at_ms,
            },
        };
        let transition = self
            .components
            .learning
            .apply(&snapshot.learning[&command.target], &input)
            .map_err(|e| domain("learning_error", e))?;
        self.commit(
            &snapshot,
            vec![
                Write::Learning {
                    target: command.target.clone(),
                    state: transition.state.clone(),
                },
                Write::Assessment(AssessmentRecord {
                    key: AssessmentKey::External {
                        provider: command.provider,
                        event_id: command.event_id,
                    },
                    dataset: snapshot.dataset.clone(),
                    target: command.target.clone(),
                    content_version: item.version,
                    input,
                    transition: transition.clone(),
                }),
                Write::Receipt(Receipt {
                    id: submission,
                    command: fingerprint,
                    result: ReceiptResult::External {
                        target: command.target.clone(),
                        transition: transition.clone(),
                    },
                }),
            ],
        )?;
        self.order_history.push(OrderEvent::Missed {
            target: command.target,
        });
        Ok(transition)
    }
    pub fn import_pgn(
        &mut self,
        dataset: &DatasetId,
        request: &ImportRequest,
    ) -> Result<Vec<tempo_chess::ContentChange>, AppError> {
        let snapshot = self.store.snapshot()?;
        if dataset != &snapshot.dataset {
            return Err(AppError::new(
                "stale_dataset",
                "import belongs to a different dataset",
            ));
        }
        let parsed = self
            .components
            .parser
            .parse(request)
            .map_err(|e| domain("content_parse_error", e))?;
        let plan = tempo_chess::plan_import(&snapshot.catalog, &parsed)
            .map_err(|e| domain("content_change_error", e))?;
        let mut writes = vec![Write::Catalog(plan.catalog.clone())];
        for change in &plan.changes {
            if matches!(change.kind, ChangeKind::Added | ChangeKind::Semantic) {
                writes.push(Write::Learning {
                    target: change.target.clone(),
                    state: LearningState::default(),
                });
            }
        }
        if let Some(id) = &snapshot.active_attempt {
            let old = &snapshot.attempts[id].attempt;
            if plan
                .catalog
                .items
                .get(&old.target)
                .is_none_or(|item| !item.active() || item.version != old.content_version)
            {
                writes.push(Write::Active(None));
            }
        }
        self.commit(&snapshot, writes)?;
        self.remaining_block.clear();
        Ok(plan.changes)
    }
    /// Accept already-fetched games. Provider I/O happens outside this application object.
    pub fn ingest_games(&mut self, games: &[Game]) -> Result<usize, AppError> {
        let mut count = 0;
        for game in games {
            let snapshot = self.store.snapshot()?;
            let deviations = self
                .components
                .comparator
                .compare(&game.pgn, &snapshot.catalog)
                .map_err(|e| domain("game_comparison_error", e))?;
            for deviation in deviations {
                let event = serde_json::to_string(&(&game.id, &deviation.target))
                    .map_err(|e| domain("codec_error", e))?;
                let version = snapshot
                    .catalog
                    .items
                    .get(&deviation.target)
                    .ok_or_else(|| {
                        AppError::new("unknown_target", "comparison referenced unknown content")
                    })?
                    .version;
                self.again(AgainCommand {
                    dataset_id: snapshot.dataset.clone(),
                    target: deviation.target,
                    expected_content_version: version,
                    provider: game.provider.clone(),
                    event_id: EventId(event),
                    observed_at_ms: game.observed_at_ms,
                })?;
                count += 1;
            }
        }
        Ok(count)
    }
    pub fn export(&self, codec: &dyn SnapshotCodec) -> Result<String, AppError> {
        let snapshot = self.store.snapshot()?;
        codec.encode(&PortableSnapshot {
            schema_version: 1,
            catalog: snapshot.catalog,
            learning: snapshot.learning,
            policy: snapshot.policy,
        })
    }
    pub fn restore(
        &mut self,
        fresh_dataset: DatasetId,
        data: &str,
        codec: &dyn SnapshotCodec,
    ) -> Result<(), AppError> {
        let snapshot = self.store.snapshot()?;
        if !snapshot.catalog.items.is_empty() {
            return Err(AppError::new(
                "restore_requires_empty",
                "restore targets an empty destination; content import is separate",
            ));
        }
        let portable = codec.decode(data)?;
        if portable.schema_version != 1 || portable.policy != self.components.learning.settings() {
            return Err(AppError::new(
                "incompatible_snapshot",
                "schema or scheduling policy differs",
            ));
        }
        if fresh_dataset.0.is_empty() {
            return Err(AppError::new(
                "invalid_dataset",
                "dataset identity is required",
            ));
        }
        for state in portable.learning.values() {
            self.components
                .learning
                .validate_state(state)
                .map_err(|e| domain("invalid_learning", e))?;
        }
        self.commit(
            &snapshot,
            vec![Write::Restore {
                dataset: fresh_dataset,
                catalog: portable.catalog,
                learning: portable.learning,
                policy: portable.policy,
            }],
        )?;
        self.remaining_block.clear();
        self.order_history.clear();
        Ok(())
    }
}

fn replay(
    snapshot: &Snapshot,
    id: &SubmissionId,
    command: &str,
) -> Result<Option<ReceiptResult>, AppError> {
    if id.0.is_empty() {
        return Err(AppError::new(
            "invalid_submission",
            "submission identity is required",
        ));
    }
    let Some(receipt) = snapshot.receipts.get(id) else {
        return Ok(None);
    };
    if receipt.command != command {
        return Err(AppError::new(
            "submission_conflict",
            "submission identity was reused with different meaning",
        ));
    }
    Ok(Some(receipt.result.clone()))
}
fn study_result(result: ReceiptResult) -> Result<AnswerResult, AppError> {
    match result {
        ReceiptResult::Study(value) => Ok(value),
        _ => Err(AppError::new("submission_conflict", "receipt kind differs")),
    }
}

struct QueueSource<'a> {
    snapshot: &'a Snapshot,
    queue: &'a dyn QueuePolicy,
    grader: &'a ChessGrader,
    today: StudyDay,
    context: QueueOrderContext,
    remaining: Vec<TargetId>,
}
impl QuestionSource for QueueSource<'_> {
    type Definition = ChessDefinition;
    type Error = AppError;
    fn next(&mut self) -> Result<Availability<Exercise<ChessDefinition>>, AppError> {
        let candidates = self
            .snapshot
            .catalog
            .items
            .values()
            .map(|item| {
                let learning = &self.snapshot.learning[&item.target];
                Candidate {
                    target: item.target.clone(),
                    active: item.active() && self.grader.start(&item.definition).is_ok(),
                    introduced: learning.introduced,
                    due: learning.due,
                    repeat_pending: learning.repeat_pending,
                    independent_new: !item.definition.is_repertoire(),
                }
            })
            .collect();
        let lines = self
            .snapshot
            .catalog
            .lines
            .iter()
            .map(|line| Line {
                id: line.id.clone(),
                targets: line.targets.clone(),
            })
            .collect();
        let eligible = self
            .queue
            .eligible(&QueueInput {
                today: self.today,
                candidates,
                lines,
            })
            .map_err(|e| domain("queue_error", e))?;
        let eligible_ids: BTreeSet<_> = eligible.targets.iter().map(|e| e.target.clone()).collect();
        self.remaining
            .retain(|target| eligible_ids.contains(target));
        let target = if !self.remaining.is_empty() {
            self.remaining.remove(0)
        } else {
            // Assign overlapping line occurrences to their first eligible block once.
            let mut assigned = BTreeSet::new();
            let groups: Vec<_> = self
                .snapshot
                .catalog
                .lines
                .iter()
                .filter_map(|line| {
                    let targets: Vec<_> = line
                        .targets
                        .iter()
                        .filter(|id| eligible_ids.contains(*id) && assigned.insert((*id).clone()))
                        .cloned()
                        .collect();
                    (!targets.is_empty()).then(|| Group {
                        id: line.id.clone(),
                        targets,
                    })
                })
                .collect();
            let blocks = self
                .queue
                .order(&eligible, &groups, &self.context)
                .map_err(|e| domain("queue_error", e))?;
            tempo_queue::validate_grouped_order(&eligible, &groups, &blocks)
                .map_err(|e| domain("queue_error", e))?;
            match tempo_queue::next(&blocks) {
                tempo_queue::Selection::Empty => return Ok(Availability::Empty),
                tempo_queue::Selection::Ready {
                    target,
                    remaining_block,
                } => {
                    self.remaining = remaining_block;
                    target
                }
            }
        };
        let item =
            self.snapshot.catalog.items.get(&target).ok_or_else(|| {
                AppError::new("queue_error", "selection referenced unknown content")
            })?;
        Ok(Availability::Ready(item.exercise()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableSnapshot {
    pub schema_version: u32,
    pub catalog: Catalog,
    pub learning: BTreeMap<TargetId, LearningState>,
    pub policy: SchedulerSettings,
}
pub trait SnapshotCodec {
    fn encode(&self, snapshot: &PortableSnapshot) -> Result<String, AppError>;
    fn decode(&self, data: &str) -> Result<PortableSnapshot, AppError>;
}
/// An executable interchange fixture. An enriched-PGN adapter will implement this same port.
pub struct JsonSnapshotCodec;
impl SnapshotCodec for JsonSnapshotCodec {
    fn encode(&self, snapshot: &PortableSnapshot) -> Result<String, AppError> {
        serde_json::to_string_pretty(snapshot).map_err(|e| domain("codec_error", e))
    }
    fn decode(&self, data: &str) -> Result<PortableSnapshot, AppError> {
        serde_json::from_str(data).map_err(|e| domain("codec_error", e))
    }
}
