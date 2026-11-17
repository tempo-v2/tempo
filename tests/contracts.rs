//! Contract journeys use the same Tempo commands that the stdio transport calls.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use tempo::wire::{StudyCommand, StudyView};
use tempo::{
    AbandonCommand, AgainCommand, Clock, Components, FixtureGames, Game, GameProvider, GameRange,
    JsonSnapshotCodec, SnapshotCodec, SubmitCommand, Tempo, UnavailableGames,
};
use tempo_chess::{Catalog, ChangeKind, ContentStatus, FixtureParser, ImportRequest, demo_catalog};
use tempo_learning::{AssessmentSource, LearningEffect, LearningState};
use tempo_queue::ReverseQueue;
use tempo_store::{
    AnswerResult, AssessmentKey, Commit, MemoryStore, Snapshot, Store, StoreError, Write,
};
use tempo_study::AttemptStatus;
use tempo_types::{
    ContentVersion, DatasetId, Grade, MissRevision, Revision, StudyDay, StudyTime, TargetId,
};

#[derive(Clone)]
struct SharedStore(Rc<RefCell<MemoryStore>>);
impl Store for SharedStore {
    fn snapshot(&self) -> Result<Snapshot, StoreError> {
        self.0.borrow().snapshot()
    }
    fn commit(&mut self, commit: Commit) -> Result<Revision, StoreError> {
        self.0.borrow_mut().commit(commit)
    }
}
struct SharedClock(Rc<Cell<StudyTime>>);
impl Clock for SharedClock {
    fn now(&self) -> StudyTime {
        self.0.get()
    }
}
struct Harness {
    app: Tempo,
    store: Rc<RefCell<MemoryStore>>,
    clock: Rc<Cell<StudyTime>>,
}
fn at(day: i32) -> StudyTime {
    StudyTime {
        day: StudyDay(day),
        unix_ms: day as u64 * 86_400_000,
    }
}
fn build(catalog: Catalog, configure: impl FnOnce(&mut Components)) -> Harness {
    let mut components = Components::fixtures();
    let clock = Rc::new(Cell::new(at(10)));
    components.clock = Box::new(SharedClock(clock.clone()));
    configure(&mut components);
    let store = Rc::new(RefCell::new(
        MemoryStore::new(
            "test-dataset".into(),
            catalog,
            components.learning.settings(),
        )
        .unwrap(),
    ));
    let app = Tempo::new(Box::new(SharedStore(store.clone())), components).unwrap();
    Harness { app, store, clock }
}
fn catalog(ids: &[&str]) -> Catalog {
    let mut result = demo_catalog();
    result.items.retain(|id, _| ids.contains(&id.0.as_str()));
    for line in &mut result.lines {
        line.targets.retain(|id| result.items.contains_key(id));
    }
    result.lines.retain(|line| !line.targets.is_empty());
    result
}
fn one(id: &str) -> Harness {
    build(catalog(&[id]), |_| {})
}
fn start(h: &mut Harness) -> AnswerResult {
    h.app.next_exercise().unwrap().expect("expected exercise")
}
fn command(prior: &AnswerResult, id: &str, answer: &str) -> SubmitCommand {
    SubmitCommand {
        dataset_id: prior.attempt.dataset.clone(),
        attempt_id: prior.attempt.id.clone(),
        submission_id: id.into(),
        expected_revision: prior.attempt.revision,
        answer: answer.into(),
    }
}
fn answer(h: &mut Harness, prior: &AnswerResult, id: &str, uci: &str) -> AnswerResult {
    h.app.submit(command(prior, id, uci)).unwrap()
}
fn external(h: &Harness, target: &str, event: &str) -> AgainCommand {
    let snapshot = h.app.snapshot().unwrap();
    AgainCommand {
        dataset_id: snapshot.dataset,
        target: target.into(),
        provider: "fixture".into(),
        event_id: event.into(),
        observed_at_ms: 0,
        expected_content_version: snapshot.catalog.items[&TargetId::from(target)].version,
    }
}
fn import_request(pgn: &str) -> ImportRequest {
    ImportRequest {
        pgn: pgn.into(),
        source: "integration-fixture".into(),
        repertoire: None,
    }
}
fn seed_due(h: &mut Harness, ids: &[&str]) {
    let snapshot = h.app.snapshot().unwrap();
    h.store
        .borrow_mut()
        .commit(Commit {
            expected_revision: snapshot.revision,
            expected_dataset: snapshot.dataset,
            writes: ids
                .iter()
                .map(|id| Write::Learning {
                    target: (*id).into(),
                    state: LearningState {
                        introduced: true,
                        due: Some(StudyDay(10)),
                        ..LearningState::default()
                    },
                })
                .collect(),
        })
        .unwrap();
}

#[test]
fn repertoire_due_journey_crosses_wire_queue_grader_learning_and_store() {
    let mut h = one("r1:p1");
    seed_due(&mut h, &["r1:p1"]);
    let StudyView::Attempt(view) = h.app.handle(StudyCommand::Next) else {
        panic!("expected attempt view")
    };
    assert_eq!(view.target_id, TargetId::from("r1:p1"));
    assert_eq!(view.status, "ready");
    let rendered = serde_json::to_value(&view).unwrap();
    assert!(rendered.get("state").is_none());
    assert!(rendered["prompt"].get("expected").is_none());
    let intent = SubmitCommand {
        dataset_id: view.dataset_id,
        attempt_id: view.attempt_id,
        submission_id: "wire-answer".into(),
        expected_revision: view.revision,
        answer: "e2e4".into(),
    };
    let encoded = serde_json::to_string(&StudyCommand::Answer(intent)).unwrap();
    let StudyView::Attempt(done) = h.app.handle(serde_json::from_str(&encoded).unwrap()) else {
        panic!("expected completion")
    };
    assert_eq!(done.status, "complete");
    assert_eq!(done.assessment, Some(Grade::Good));
    let saved = h.app.snapshot().unwrap();
    assert_eq!(saved.assessments.len(), 1);
    assert_eq!(
        saved.learning[&TargetId::from("r1:p1")].due,
        Some(StudyDay(13))
    );
    assert_eq!(
        saved.learning[&TargetId::from("r1:p1")].last_credit_day,
        Some(StudyDay(10))
    );
    assert!(matches!(
        h.app.handle(StudyCommand::Next),
        StudyView::Empty(_)
    ));
}

#[test]
fn two_decision_tactic_has_one_grade_and_autoplay_has_none() {
    let mut h = one("t1");
    let issued = start(&mut h);
    let first = answer(&mut h, &issued, "t-first", "e2e4");
    assert_eq!(first.assessment, None);
    assert_eq!(first.attempt.prompt.as_ref().unwrap().decision_index, 1);
    assert_eq!(
        first.feedback.as_ref().unwrap().autoplay,
        vec!["e7e5".into()]
    );
    let half = h.app.snapshot().unwrap();
    assert!(half.assessments.is_empty());
    assert!(!half.learning[&TargetId::from("t1")].introduced);
    let final_answer = answer(&mut h, &first, "t-final", "g1f3");
    assert_eq!(final_answer.assessment, Some(Grade::Good));
    assert_eq!(final_answer.attempt.status, AttemptStatus::Complete);
    assert_eq!(h.app.snapshot().unwrap().assessments.len(), 1);
}

#[test]
fn first_miss_is_committed_correction_stays_failed_and_new_repeat_preserves_tomorrow() {
    let mut h = one("t1");
    let issued = start(&mut h);
    let missed = answer(&mut h, &issued, "miss", "e2e3");
    assert_eq!(missed.assessment, Some(Grade::Again));
    assert_eq!(missed.attempt.status, AttemptStatus::Active);
    let persisted = h.app.snapshot().unwrap();
    assert_eq!(persisted.assessments.len(), 1);
    assert!(persisted.learning[&TargetId::from("t1")].repeat_pending);
    assert_eq!(
        h.app.resume(&issued.attempt.id).unwrap().attempt.terminal,
        Some(Grade::Again)
    );
    let corrected = answer(&mut h, &missed, "correct-1", "e2e4");
    let finished = answer(&mut h, &corrected, "correct-2", "g1f3");
    assert_eq!(finished.attempt.terminal, Some(Grade::Again));
    assert_eq!(finished.assessment, None);
    assert_eq!(h.app.snapshot().unwrap().assessments.len(), 1);
    let fresh = start(&mut h);
    assert_ne!(fresh.attempt.id, issued.attempt.id);
    let step = answer(&mut h, &fresh, "repeat-1", "e2e4");
    let done = answer(&mut h, &step, "repeat-2", "g1f3");
    assert_eq!(done.learning_effect, Some(LearningEffect::RepeatCleared));
    let state = &h.app.snapshot().unwrap().learning[&TargetId::from("t1")];
    assert_eq!(state.due, Some(StudyDay(11)));
    assert!(!state.repeat_pending);
    assert_eq!(state.miss_revision, MissRevision(1));
    assert_eq!(state.scheduled_reviews, 1);
    assert!(h.app.next_exercise().unwrap().is_none());
}

#[test]
fn active_resume_replay_and_revision_checks_prevent_double_answers() {
    let mut h = one("t1");
    let issued = start(&mut h);
    let before = h.app.snapshot().unwrap();
    assert_eq!(h.app.next_exercise().unwrap().unwrap(), issued);
    assert_eq!(h.app.resume(&issued.attempt.id).unwrap(), issued);
    assert_eq!(h.app.snapshot().unwrap(), before);
    let first_command = command(&issued, "same-command", "e2e4");
    let first = h.app.submit(first_command.clone()).unwrap();
    let accepted = h.app.snapshot().unwrap();
    assert_eq!(h.app.submit(first_command.clone()).unwrap(), first);
    assert_eq!(h.app.snapshot().unwrap(), accepted);
    let mut conflicting = first_command.clone();
    conflicting.answer = "d2d4".into();
    assert_eq!(
        h.app.submit(conflicting).unwrap_err().code,
        "submission_conflict"
    );
    let mut stale_question = first_command;
    stale_question.submission_id = "different-command-old-question".into();
    assert_eq!(
        h.app.submit(stale_question).unwrap_err().code,
        "invalid_submission"
    );
    let completed = answer(&mut h, &first, "finish", "g1f3");
    assert_eq!(
        h.app
            .submit(command(&completed, "second-terminal-id", "g1f3"))
            .unwrap_err()
            .code,
        "invalid_submission"
    );
    assert_eq!(h.app.snapshot().unwrap().assessments.len(), 1);
}

#[test]
fn failed_commit_leaves_progress_grade_learning_and_receipt_unchanged_then_retry_works() {
    let mut h = one("t1");
    let issued = start(&mut h);
    let before = h.app.snapshot().unwrap();
    let submit = command(&issued, "retry-after-failure", "e2e3");
    h.store.borrow_mut().fail_next_commit();
    let failure = h.app.submit(submit.clone()).unwrap_err();
    assert_eq!(failure.code, "store_unavailable");
    assert!(failure.retryable);
    assert_eq!(h.app.snapshot().unwrap(), before);
    let accepted = h.app.submit(submit.clone()).unwrap();
    assert_eq!(accepted.assessment, Some(Grade::Again));
    let saved = h.app.snapshot().unwrap();
    assert_eq!(saved.assessments.len(), 1);
    assert_eq!(saved.receipts.len(), 1);
    assert_eq!(h.app.submit(submit).unwrap(), accepted);
    assert_eq!(h.app.snapshot().unwrap(), saved);
}

#[test]
fn programmatic_again_has_source_identity_and_no_fabricated_attempt() {
    let mut h = one("r1:p1");
    let command = external(&h, "r1:p1", "game-42");
    let applied = h.app.again(command.clone()).unwrap();
    assert_eq!(applied.effect, LearningEffect::MissRecorded);
    assert_eq!(applied.state.due, Some(StudyDay(11)));
    let snapshot = h.app.snapshot().unwrap();
    assert!(snapshot.attempts.is_empty());
    assert_eq!(snapshot.assessments.len(), 1);
    assert!(matches!(
        &snapshot.assessments[0].key,
        AssessmentKey::External { .. }
    ));
    assert!(matches!(
        &snapshot.assessments[0].input.source,
        AssessmentSource::External {
            observed_at_ms: 0,
            ..
        }
    ));
    assert_eq!(snapshot.assessments[0].input.accepted_at, at(10));
    assert_eq!(h.app.again(command.clone()).unwrap(), applied);
    assert_eq!(h.app.snapshot().unwrap(), snapshot);
    let mut reused = command;
    reused.observed_at_ms = 1;
    assert_eq!(h.app.again(reused).unwrap_err().code, "submission_conflict");
}

#[test]
fn old_human_success_cannot_clear_a_newer_external_miss() {
    let mut h = one("r1:p1");
    let issued = start(&mut h);
    h.app.again(external(&h, "r1:p1", "newer-miss")).unwrap();
    let old_command = command(&issued, "old-success", "e2e4");
    let stale = h.app.submit(old_command.clone()).unwrap();
    assert_eq!(stale.assessment, Some(Grade::Good));
    assert_eq!(
        stale.learning_effect,
        Some(LearningEffect::StalePositiveIgnored)
    );
    assert!(h.app.snapshot().unwrap().learning[&TargetId::from("r1:p1")].repeat_pending);
    let fresh = start(&mut h);
    let cleared = answer(&mut h, &fresh, "fresh-success", "e2e4");
    assert_eq!(cleared.learning_effect, Some(LearningEffect::RepeatCleared));
    let after = h.app.snapshot().unwrap();
    h.clock.set(at(11));
    assert_eq!(h.app.submit(old_command).unwrap(), stale);
    assert_eq!(h.app.snapshot().unwrap(), after);
}

#[test]
fn metadata_import_preserves_learning_version_and_pending_attempt() {
    let mut metadata = catalog(&["r1:p1"]);
    metadata
        .items
        .get_mut(&TargetId::from("r1:p1"))
        .unwrap()
        .label = "Updated explanation".into();
    let mut h = build(catalog(&["r1:p1"]), |c| {
        c.parser = Box::new(FixtureParser::default().with_fixture("metadata", metadata));
    });
    seed_due(&mut h, &["r1:p1"]);
    let issued = start(&mut h);
    let old_learning = h.app.snapshot().unwrap().learning;
    let changes = h
        .app
        .import_pgn(&issued.attempt.dataset, &import_request("metadata"))
        .unwrap();
    assert!(changes.iter().any(|c| c.kind == ChangeKind::Metadata));
    let snapshot = h.app.snapshot().unwrap();
    assert_eq!(snapshot.learning, old_learning);
    assert_eq!(
        snapshot.catalog.items[&TargetId::from("r1:p1")].version,
        ContentVersion(1)
    );
    assert_eq!(
        answer(&mut h, &issued, "after-metadata", "e2e4").assessment,
        Some(Grade::Good)
    );
}

#[test]
fn semantic_update_rejects_pending_and_stale_game_work_but_replays_committed_receipt() {
    let mut changed = catalog(&["r1:p1"]);
    changed
        .items
        .get_mut(&TargetId::from("r1:p1"))
        .unwrap()
        .definition
        .decisions[0]
        .expected = "d2d4".into();
    let mut h = build(catalog(&["r1:p1"]), |c| {
        c.parser = Box::new(FixtureParser::default().with_fixture("replacement", changed));
    });
    let issued = start(&mut h);
    let committed_command = command(&issued, "accepted-old-content", "e2e4");
    let accepted = h.app.submit(committed_command.clone()).unwrap();
    h.clock.set(at(13));
    let pending = start(&mut h);
    let stale_game = external(&h, "r1:p1", "compared-before-edit");
    h.app
        .import_pgn(&pending.attempt.dataset, &import_request("replacement"))
        .unwrap();
    let replacement = h.app.snapshot().unwrap();
    assert_eq!(
        replacement.catalog.items[&TargetId::from("r1:p1")].version,
        ContentVersion(2)
    );
    assert_eq!(
        replacement.learning[&TargetId::from("r1:p1")],
        LearningState::default()
    );
    assert_eq!(
        h.app
            .submit(command(&pending, "pending-old-content", "e2e4"))
            .unwrap_err()
            .code,
        "stale_content"
    );
    assert_eq!(h.app.again(stale_game).unwrap_err().code, "stale_content");
    assert_eq!(h.app.submit(committed_command).unwrap(), accepted);
    assert_eq!(h.app.snapshot().unwrap(), replacement);
    assert_eq!(replacement.assessments.len(), 1);
    assert_eq!(replacement.content_history.len(), 2);
    let new = start(&mut h);
    assert_eq!(
        answer(&mut h, &new, "new-content", "d2d4").assessment,
        Some(Grade::Good)
    );
}

#[test]
fn ordinary_import_preserves_progress_and_portable_restore_preserves_next_grade() {
    let mut source = one("r1:p1");
    let issued = start(&mut source);
    let missed = answer(&mut source, &issued, "miss-before-export", "d2d4");
    answer(&mut source, &missed, "correction-before-export", "e2e4");
    let fresh = start(&mut source);
    answer(&mut source, &fresh, "repeat-before-export", "e2e4");
    let learning = source.app.snapshot().unwrap().learning;
    let dataset = source.app.snapshot().unwrap().dataset;
    source
        .app
        .import_pgn(&dataset, &import_request(tempo_chess::REPERTOIRE_PGN))
        .unwrap();
    assert_eq!(
        source.app.snapshot().unwrap().learning[&TargetId::from("r1:p1")],
        learning[&TargetId::from("r1:p1")]
    );
    let portable = source.app.export(&JsonSnapshotCodec).unwrap();
    let mut destination = build(Catalog::default(), |_| {});
    destination
        .app
        .restore("restored-dataset".into(), &portable, &JsonSnapshotCodec)
        .unwrap();
    let restored = destination.app.snapshot().unwrap();
    assert_eq!(restored.catalog, source.app.snapshot().unwrap().catalog);
    assert_eq!(restored.learning, source.app.snapshot().unwrap().learning);
    assert_eq!(restored.dataset, DatasetId::from("restored-dataset"));
    assert!(restored.attempts.is_empty());
    source.clock.set(at(11));
    destination.clock.set(at(11));
    let from_source = start(&mut source);
    let from_restore = start(&mut destination);
    assert_eq!(from_source.attempt.target, from_restore.attempt.target);
    let mut old_dataset = command(&from_restore, "old-dataset-command", "e2e4");
    old_dataset.dataset_id = dataset;
    assert_eq!(
        destination.app.submit(old_dataset).unwrap_err().code,
        "stale_dataset"
    );
    answer(&mut source, &from_source, "after-export", "e2e4");
    answer(&mut destination, &from_restore, "after-restore", "e2e4");
    assert_eq!(
        source.app.snapshot().unwrap().learning,
        destination.app.snapshot().unwrap().learning
    );
}

#[test]
fn restore_rejects_bad_settings_memory_and_reused_dataset_before_mutation() {
    let mut source = one("r1:p1");
    let issued = start(&mut source);
    answer(&mut source, &issued, "snapshot-grade", "e2e4");
    let encoded = source.app.export(&JsonSnapshotCodec).unwrap();
    let portable = JsonSnapshotCodec.decode(&encoded).unwrap();
    let mut destination = build(Catalog::default(), |_| {});
    let before = destination.app.snapshot().unwrap();
    let mut wrong_policy = portable.clone();
    wrong_policy.policy.parameters = 9u32.to_le_bytes().to_vec();
    let error = destination
        .app
        .restore(
            "fresh".into(),
            &JsonSnapshotCodec.encode(&wrong_policy).unwrap(),
            &JsonSnapshotCodec,
        )
        .unwrap_err();
    assert_eq!(error.code, "incompatible_snapshot");
    assert_eq!(destination.app.snapshot().unwrap(), before);
    let mut wrong_memory = portable;
    wrong_memory
        .learning
        .get_mut(&TargetId::from("r1:p1"))
        .unwrap()
        .scheduler_memory
        .as_mut()
        .unwrap()
        .schema_version = 99;
    assert_eq!(
        destination
            .app
            .restore(
                "fresh".into(),
                &JsonSnapshotCodec.encode(&wrong_memory).unwrap(),
                &JsonSnapshotCodec
            )
            .unwrap_err()
            .code,
        "invalid_learning"
    );
    assert_eq!(destination.app.snapshot().unwrap(), before);
    assert_eq!(
        destination
            .app
            .restore(before.dataset.clone(), &encoded, &JsonSnapshotCodec)
            .unwrap_err()
            .code,
        "store_conflict"
    );
    assert_eq!(destination.app.snapshot().unwrap(), before);
    destination
        .app
        .restore("fresh".into(), &encoded, &JsonSnapshotCodec)
        .unwrap();
    assert_eq!(
        destination
            .app
            .restore("another".into(), &encoded, &JsonSnapshotCodec)
            .unwrap_err()
            .code,
        "restore_requires_empty"
    );
}

#[test]
fn optional_game_failure_does_not_block_study_and_scan_uses_again_with_replay() {
    let mut unavailable = one("r1:p1");
    let range = GameRange {
        from_ms: 0,
        to_ms: 100,
    };
    assert_eq!(
        UnavailableGames.fetch(&range).unwrap_err().code,
        "provider_unavailable"
    );
    assert_eq!(
        start(&mut unavailable).attempt.target,
        TargetId::from("r1:p1")
    );
    let mut available = one("r1:p1");
    let games = FixtureGames(vec![Game {
        provider: "fixture".into(),
        id: "game-1".into(),
        observed_at_ms: 1,
        pgn: tempo_chess::GAME_PGN.into(),
    }])
    .fetch(&range)
    .unwrap();
    assert_eq!(available.app.ingest_games(&games).unwrap(), 1);
    let first = available.app.snapshot().unwrap();
    assert!(first.attempts.is_empty());
    assert_eq!(first.assessments.len(), 1);
    assert!(first.learning[&TargetId::from("r1:p1")].repeat_pending);
    assert_eq!(available.app.ingest_games(&games).unwrap(), 1);
    assert_eq!(available.app.snapshot().unwrap(), first);
}

#[test]
fn queue_policy_substitution_changes_selection_and_preserves_group_contiguity() {
    let mut stable = build(demo_catalog(), |_| {});
    let mut reverse = build(demo_catalog(), |c| c.queue = Box::new(ReverseQueue));
    assert_eq!(start(&mut stable).attempt.target, TargetId::from("r1:p1"));
    assert_eq!(start(&mut reverse).attempt.target, TargetId::from("t1"));
    assert_eq!(
        stable.app.snapshot().unwrap().learning,
        reverse.app.snapshot().unwrap().learning
    );
    let mut grouped = build(demo_catalog(), |_| {});
    seed_due(&mut grouped, &["r1:p1", "r1:p2", "t1"]);
    let first = start(&mut grouped);
    assert_eq!(first.attempt.target, TargetId::from("r1:p1"));
    answer(&mut grouped, &first, "group-first", "e2e4");
    grouped
        .app
        .again(external(&grouped, "t1", "new-priority-miss"))
        .unwrap();
    assert_eq!(start(&mut grouped).attempt.target, TargetId::from("r1:p2"));
}

#[test]
fn unknown_content_is_rejected_and_quarantined_items_do_not_block_valid_study() {
    let mut content = demo_catalog();
    content.items.get_mut(&TargetId::from("t1")).unwrap().status = ContentStatus::Quarantined;
    let mut h = build(content, |_| {});
    let before = h.app.snapshot().unwrap();
    assert_eq!(
        h.app
            .import_pgn(&before.dataset, &import_request("not a registered fixture"))
            .unwrap_err()
            .code,
        "content_parse_error"
    );
    assert_eq!(h.app.snapshot().unwrap(), before);
    assert_eq!(start(&mut h).attempt.target, TargetId::from("r1:p1"));
    let mut only_quarantined = catalog(&["t1"]);
    only_quarantined
        .items
        .get_mut(&TargetId::from("t1"))
        .unwrap()
        .status = ContentStatus::Quarantined;
    let mut empty = build(only_quarantined, |_| {});
    assert!(empty.app.next_exercise().unwrap().is_none());
}

#[test]
fn abandonment_is_replayable_without_a_grade_and_a_fresh_attempt_can_start() {
    let mut h = one("t1");
    let issued = start(&mut h);
    let abandon = AbandonCommand {
        dataset_id: issued.attempt.dataset,
        attempt_id: issued.attempt.id.clone(),
        submission_id: "abandon".into(),
        expected_revision: issued.attempt.revision,
    };
    let ended = h.app.abandon(abandon.clone()).unwrap();
    assert_eq!(ended.attempt.status, AttemptStatus::Abandoned);
    assert_eq!(ended.assessment, None);
    let snapshot = h.app.snapshot().unwrap();
    assert!(snapshot.assessments.is_empty());
    assert_eq!(
        snapshot.learning[&TargetId::from("t1")],
        LearningState::default()
    );
    assert_eq!(h.app.abandon(abandon).unwrap(), ended);
    assert_eq!(h.app.snapshot().unwrap(), snapshot);
    assert_ne!(start(&mut h).attempt.id, issued.attempt.id);
}
