use super::*;
use crate::{
    FixedClock, SubmitCommand,
    game_detection::{AnalysisOptions, DetectorRegistry, GameDetection},
    game_evidence::GameImport,
    local_runtime,
    tactical_jobs::{TacticalJobs, TacticalRead},
};
use std::time::{Duration, Instant};
use tempo_chess::DetectorDescriptor;
use tempo_chess::{
    DetectionControl, EvaluationFailure, EvaluationProvenance, EvaluationScore,
    HangingMaterialConfig, HangingMaterialDetector, RootEvaluation, RootScore, ScoreBound,
    TacticRecord, TacticalEvaluationRequest, TacticalEvaluator, compile_tactics,
};
use tempo_store::{ActivityMode, Commit, Store, Write};

const HANGING: &str = "7k/n7/8/8/8/8/8/R5K1 w - - 0 1";

struct Evaluator {
    revision: &'static str,
    outcome: &'static str,
}
impl TacticalEvaluator for Evaluator {
    fn descriptor(&self) -> EvaluationProvenance {
        EvaluationProvenance {
            engine: "Consumer fixture".into(),
            engine_version: self.revision.into(),
            configuration_fingerprint: self.outcome.into(),
        }
    }
    fn evaluate(
        &self,
        request: &TacticalEvaluationRequest,
        _: &dyn DetectionControl,
    ) -> Result<RootEvaluation, EvaluationFailure> {
        if self.outcome == "uncertain" {
            return Err(EvaluationFailure::Incomplete);
        }
        Ok(RootEvaluation {
            provenance: self.descriptor(),
            depth: 12,
            nodes: 50_000,
            roots: request
                .legal_moves
                .iter()
                .map(|uci| RootScore {
                    uci: uci.clone(),
                    bound: ScoreBound::Exact,
                    score: EvaluationScore::Centipawns(match (self.outcome, uci.as_str()) {
                        ("different", "d4e5") => 1000,
                        ("different", _) => 0,
                        ("inferior", "a1a7") => -1000,
                        (_, "a1a7") => 1000,
                        _ => 0,
                    }),
                })
                .collect(),
        })
    }
}

fn host(revision: &'static str, outcome: &'static str) -> (GameDetection, TacticalJobs) {
    let mut registry = DetectorRegistry::default();
    registry
        .register(
            HangingMaterialDetector::new(
                Evaluator { revision, outcome },
                HangingMaterialConfig::default(),
            )
            .unwrap(),
        )
        .unwrap();
    let detection = GameDetection::new(
        registry,
        AnalysisOptions {
            deadline: Duration::from_secs(5),
            retained_results: 8,
        },
    )
    .unwrap();
    let jobs = TacticalJobs::new(&detection, &["hanging-material"]);
    (detection, jobs)
}

fn disposable() -> (tempfile::TempDir, Tempo) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tactics.sqlite");
    local_runtime::init(&path).unwrap();
    local_runtime::configure(&path, 0, 0, None).unwrap();
    let mut app = local_runtime::open(&path).unwrap();
    app.components.clock = Box::new(FixedClock(app.components.clock.now()));
    (directory, app)
}

fn seed_due(app: &mut Tempo, label: &str) -> tempo_types::TargetId {
    seed_due_with_answer(app, label, "e2e4")
}

fn seed_due_with_answer(app: &mut Tempo, label: &str, answer: &str) -> tempo_types::TargetId {
    let snapshot = app.store.content_state().unwrap();
    let fresh = compile_tactics(&[TacticRecord {
        namespace: "fixture".into(),
        source_id: answer.into(),
        solver_fen: tempo_chess::START_FEN.into(),
        solution: vec![answer.into()],
        label: label.into(),
        provenance: vec![],
    }])
    .unwrap();
    let target = fresh.items.keys().next().unwrap().clone();
    let mut catalog = snapshot.catalog;
    catalog.items.extend(fresh.items);
    let state =
        DailyLearning::introduce_capture(&Default::default(), app.components.clock.now()).unwrap();
    app.store
        .commit(Commit {
            expected_revision: snapshot.revision,
            expected_dataset: snapshot.dataset,
            writes: vec![
                Write::Catalog(catalog),
                Write::Learning {
                    target: target.clone(),
                    state,
                },
            ],
        })
        .unwrap();
    target
}

fn import(app: &mut Tempo, opponent: &str, line: &str) {
    let result = app.import_game_pgn(GameImport {
        dataset_id: app.game_dataset().unwrap(), command_id: format!("import-{}", opponent.replace(' ', "-")), learner: "Learner".into(), repertoire_id: None,
        pgn: format!("[White \"Learner\"]\n[Black \"{opponent}\"]\n[SetUp \"1\"]\n[FEN \"{HANGING}\"]\n[Result \"0-1\"]\n\n{line} 0-1\n"),
    }).unwrap();
    assert_eq!(result.new_games, 1);
}

fn read(app: &Tempo, jobs: &TacticalJobs, store: &tempo_store::SqliteStore) -> Value {
    jobs.read(
        app,
        store,
        TacticalRead {
            dataset_id: app.game_dataset().unwrap(),
            before: None,
        },
        false,
    )
    .unwrap()
}

fn scan(
    directory: &tempfile::TempDir,
    app: &mut Tempo,
    detection: &mut GameDetection,
    jobs: &mut TacticalJobs,
    games: u64,
) -> Value {
    let store = tempo_store::SqliteStore::open(directory.path().join("tactics.sqlite")).unwrap();
    jobs.force_idle_for_test();
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        detection.poll(app);
        jobs.poll(app, &store, detection);
        let result = read(app, jobs, &store);
        if result["statistics"]["completed_games"] == games
            && result["statistics"]["pending_games"] == 0
        {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "tactical consumer did not settle: {result}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn answer(
    app: &mut Tempo,
    issued: &tempo_store::AnswerResult,
    uci: &str,
    id: &str,
) -> SubmitCommand {
    let command = SubmitCommand {
        dataset_id: issued.attempt.dataset.clone(),
        attempt_id: issued.attempt.id.clone(),
        expected_revision: issued.attempt.revision,
        submission_id: id.into(),
        answer: uci.into(),
    };
    app.submit(command.clone()).unwrap();
    command
}

#[test]
fn accepted_miss_preserves_active_then_is_next_independently_of_display_name() {
    for label in ["Ordinary tactic", "Renamed display only", ""] {
        let (directory, mut app) = disposable();
        let normal = seed_due(&mut app, label);
        let first = app.next_exercise().unwrap().unwrap();
        assert_eq!(first.attempt.target, normal);
        let original = app
            .store
            .stored_attempt(&first.attempt.id)
            .unwrap()
            .unwrap();
        let (mut detection, mut jobs) = host("1", "miss");
        import(&mut app, "First opponent", "1. Kg2 Kg7");
        let report = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
        assert_eq!(report["statistics"]["outcomes"]["missed_good_capture"], 1);
        assert_eq!(report["statistics"]["decisions_scanned"], 1);
        assert_eq!(report["observations"][0]["ply"], 0);
        assert_eq!(
            app.store
                .stored_attempt(&first.attempt.id)
                .unwrap()
                .unwrap(),
            original
        );
        assert_eq!(app.next_exercise().unwrap().unwrap().attempt, first.attempt);

        let command = answer(&mut app, &first, "e2e4", "ordinary-answer");
        let accepted = app.submit(command.clone()).unwrap();
        let revision = app.snapshot().unwrap().revision;
        assert_eq!(app.submit(command).unwrap(), accepted);
        assert_eq!(app.snapshot().unwrap().revision, revision);

        let urgent = app.next_exercise().unwrap().unwrap();
        assert_ne!(urgent.attempt.target, normal);
        assert_eq!(urgent.attempt.prompt.as_ref().unwrap().fen, HANGING);
        let snapshot = app.snapshot().unwrap();
        let item = &snapshot.catalog.items[&urgent.attempt.target];
        assert_eq!(item.definition.decisions.len(), 1);
        assert_eq!(item.definition.decisions[0].expected.uci, "a1a7");
        assert_eq!(
            snapshot.attempts[&urgent.attempt.id].mode,
            ActivityMode::Test
        );
        assert_eq!(
            snapshot.learning[&urgent.attempt.target].first_introduced,
            Some(app.components.clock.now().day)
        );
    }
}

#[test]
fn later_occurrence_survives_old_completion_reuses_learning_and_replays_after_restart() {
    let (directory, mut app) = disposable();
    let (mut detection, mut jobs) = host("1", "miss");
    import(&mut app, "First opponent", "1. Kg2 Kg7");
    scan(&directory, &mut app, &mut detection, &mut jobs, 1);
    let first = app.next_exercise().unwrap().unwrap();
    import(&mut app, "Second opponent", "1. Kg2 Kg7");
    let report = scan(&directory, &mut app, &mut detection, &mut jobs, 2);
    assert_eq!(report["statistics"]["observations"], 2);
    assert_eq!(app.snapshot().unwrap().catalog.items.len(), 1);
    assert_eq!(app.next_exercise().unwrap().unwrap().attempt, first.attempt);

    let original_command = answer(&mut app, &first, "a1a7", "first-puzzle-answer");
    let first_saved = app.submit(original_command.clone()).unwrap();
    let learned = app.snapshot().unwrap().learning[&first.attempt.target].clone();
    let second = app.next_exercise().unwrap().unwrap();
    assert_eq!(second.attempt.target, first.attempt.target);
    assert_ne!(second.attempt.id, first.attempt.id);
    assert_eq!(
        app.snapshot().unwrap().attempts[&second.attempt.id].mode,
        ActivityMode::Practice
    );
    let practice = answer(&mut app, &second, "a1a7", "second-puzzle-answer");
    assert_eq!(app.submit(practice).unwrap().assessment, None);
    assert_eq!(
        app.snapshot().unwrap().learning[&first.attempt.target],
        learned
    );
    assert!(app.next_exercise().unwrap().is_none());

    // A detector revision revises the two observations and never issues again
    // for occurrences whose requests were already fulfilled.
    let (mut revised_detection, mut revised_jobs) = host("2", "miss");
    let report = scan(
        &directory,
        &mut app,
        &mut revised_detection,
        &mut revised_jobs,
        2,
    );
    assert_eq!(report["statistics"]["observations"], 2);
    assert_eq!(report["statistics"]["pending_requests"], 0);
    assert!(app.next_exercise().unwrap().is_none());
    drop(app);
    let mut restored = local_runtime::open(&directory.path().join("tactics.sqlite")).unwrap();
    assert_eq!(restored.submit(original_command).unwrap(), first_saved);
    assert_eq!(
        restored.snapshot().unwrap().learning[&first.attempt.target],
        learned
    );
    assert!(restored.next_exercise().unwrap().is_none());
}

#[test]
fn inferior_or_uncertain_capture_is_report_only_and_scan_replay_is_read_only() {
    for (kind, expected) in [
        ("inferior", "declined_inferior_capture"),
        ("uncertain", "uncertain"),
    ] {
        let (directory, mut app) = disposable();
        let (mut detection, mut jobs) = host("1", kind);
        import(&mut app, "Report opponent", "1. Kg2 Kg7");
        let report = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
        assert_eq!(report["statistics"]["outcomes"][expected], 1);
        assert_eq!(report["statistics"]["pending_requests"], 0);
        assert!(app.snapshot().unwrap().catalog.items.is_empty());
        assert!(app.next_exercise().unwrap().is_none());
        let before = app.snapshot().unwrap();
        let _ = read(
            &app,
            &jobs,
            &tempo_store::SqliteStore::open(directory.path().join("tactics.sqlite")).unwrap(),
        );
        import(&mut app, "Report opponent", "1. Kg2 Kg7");
        assert_eq!(app.snapshot().unwrap(), before);
    }
}

#[test]
fn skip_defers_urgent_practice_but_a_fresh_miss_restores_priority() {
    let (directory, mut app) = disposable();
    let (mut detection, mut jobs) = host("1", "miss");
    import(&mut app, "First opponent", "1. Kg2 Kg7");
    scan(&directory, &mut app, &mut detection, &mut jobs, 1);
    let first = app.next_exercise().unwrap().unwrap();
    answer(&mut app, &first, "a1a7", "scheduled-answer");
    let learned = app.snapshot().unwrap().learning[&first.attempt.target].clone();

    import(&mut app, "Second opponent", "1. Kg2 Kg7");
    scan(&directory, &mut app, &mut detection, &mut jobs, 2);
    let practice = app.next_exercise().unwrap().unwrap();
    assert_eq!(
        app.snapshot().unwrap().attempts[&practice.attempt.id].mode,
        ActivityMode::Practice
    );
    let status =
        serde_json::to_value(app.handle(crate::commands::StudyCommand::StudyStatus)).unwrap();
    assert_eq!(status["done"], false);
    assert_eq!(status["outstanding"], 1);

    let skip = crate::AbandonCommand {
        dataset_id: practice.attempt.dataset.clone(),
        attempt_id: practice.attempt.id.clone(),
        expected_revision: practice.attempt.revision,
        submission_id: "skip-urgent-practice".into(),
    };
    let skipped = app.skip(skip.clone()).unwrap();
    assert!(app.next_exercise().unwrap().is_none());
    assert_eq!(app.skip(skip.clone()).unwrap(), skipped);
    assert!(app.next_exercise().unwrap().is_none());

    // A configuration revision is the same occurrence, not a new miss. It must
    // neither reset the explicit session deferral nor issue another request.
    let (mut revised_detection, mut revised_jobs) = host("2", "miss");
    let report = scan(
        &directory,
        &mut app,
        &mut revised_detection,
        &mut revised_jobs,
        2,
    );
    assert_eq!(report["statistics"]["pending_requests"], 1);
    assert!(app.next_exercise().unwrap().is_none());

    // A distinct game occurrence is fresh work. Replaying an old Skip afterward
    // must not defer that new request, and Practice must retain the schedule.
    import(&mut app, "Third opponent", "1. Kg2 Kg7");
    let report = scan(
        &directory,
        &mut app,
        &mut revised_detection,
        &mut revised_jobs,
        3,
    );
    assert_eq!(report["statistics"]["pending_requests"], 2);
    assert_eq!(app.skip(skip).unwrap(), skipped);
    let next = app.next_exercise().unwrap().unwrap();
    assert_eq!(next.attempt.target, first.attempt.target);
    assert_ne!(next.attempt.id, practice.attempt.id);
    assert_eq!(
        app.snapshot().unwrap().learning[&first.attempt.target],
        learned
    );
}

#[test]
fn failed_and_timed_out_scans_do_not_block_other_games_or_create_false_misses() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempo_chess::{DetectionOutcome, DetectionStop, GameDetectionInput, GameDetector};

    struct FailFirst {
        inner: HangingMaterialDetector<Evaluator>,
        pending: AtomicBool,
        timed_out: bool,
    }
    impl GameDetector for FailFirst {
        fn produces_tactical_assessments(&self) -> bool {
            true
        }
        fn descriptor(&self) -> DetectorDescriptor {
            self.inner.descriptor()
        }
        fn detect(
            &self,
            input: &GameDetectionInput,
            control: &dyn DetectionControl,
        ) -> DetectionOutcome {
            if self.pending.swap(false, Ordering::SeqCst) {
                if self.timed_out {
                    DetectionOutcome::Stopped(DetectionStop::Deadline)
                } else {
                    DetectionOutcome::Failed("fixture engine failure".into())
                }
            } else {
                self.inner.detect(input, control)
            }
        }
        fn detect_position(
            &self,
            position: &tempo_chess::PositionPass<'_>,
            control: &dyn DetectionControl,
        ) -> DetectionOutcome {
            if self.pending.swap(false, Ordering::SeqCst) {
                if self.timed_out {
                    DetectionOutcome::Stopped(DetectionStop::Deadline)
                } else {
                    DetectionOutcome::Failed("fixture engine failure".into())
                }
            } else {
                self.inner.detect_position(position, control)
            }
        }
    }
    for timed_out in [false, true] {
        let (directory, mut app) = disposable();
        import(&mut app, "Older opponent", "1. Kg2 Kg7");
        import(&mut app, "Newer opponent", "1. Kg2 Kg7");
        let mut registry = DetectorRegistry::default();
        registry
            .register(FailFirst {
                inner: HangingMaterialDetector::new(
                    Evaluator {
                        revision: "1",
                        outcome: "miss",
                    },
                    HangingMaterialConfig::default(),
                )
                .unwrap(),
                pending: AtomicBool::new(true),
                timed_out,
            })
            .unwrap();
        let mut detection = GameDetection::new(
            registry,
            AnalysisOptions {
                deadline: Duration::from_secs(5),
                retained_results: 8,
            },
        )
        .unwrap();
        let mut jobs = TacticalJobs::new(&detection, &["hanging-material"]);
        let report = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
        let failure = if timed_out {
            "timed_out_games"
        } else {
            "failed_games"
        };
        assert_eq!(report["statistics"][failure], 1);
        assert_eq!(report["statistics"]["outcomes"]["missed_good_capture"], 1);
        assert_eq!(report["statistics"]["pending_requests"], 1);
        assert_eq!(report["statistics"]["decisions_scanned"], 1);
        assert_eq!(app.snapshot().unwrap().catalog.items.len(), 1);
    }
}

#[test]
fn deterministic_acceptance_failure_is_recorded_without_blocking_the_archive() {
    let (directory, mut app) = disposable();
    let snapshot = app.store.content_state().unwrap();
    let mut catalog = compile_tactics(&[TacticRecord {
        namespace: "retained".into(),
        source_id: "retired".into(),
        solver_fen: HANGING.into(),
        solution: vec!["a1a7".into()],
        label: "Retained retired tactic".into(),
        provenance: vec![],
    }])
    .unwrap();
    for item in catalog.items.values_mut() {
        item.status = tempo_chess::ContentStatus::Retired;
    }
    let mut writes = vec![Write::Catalog(catalog.clone())];
    writes.extend(catalog.items.keys().map(|target| Write::Learning {
        target: target.clone(),
        state: Default::default(),
    }));
    app.store
        .commit(Commit {
            expected_revision: snapshot.revision,
            expected_dataset: snapshot.dataset,
            writes,
        })
        .unwrap();
    let retained_learning = app.snapshot().unwrap().learning;
    import(&mut app, "Older captured", "1. Rxa7 Kg8");
    import(&mut app, "Newer missed", "1. Kg2 Kg7");
    let (mut detection, mut jobs) = host("1", "miss");
    let report = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
    assert_eq!(report["statistics"]["failed_games"], 1);
    assert_eq!(report["statistics"]["outcomes"]["took_good_capture"], 1);
    assert_eq!(report["statistics"]["pending_requests"], 0);
    assert_eq!(app.snapshot().unwrap().catalog, catalog);
    assert_eq!(app.snapshot().unwrap().learning, retained_learning);
    assert!(app.next_exercise().unwrap().is_none());
}

#[test]
fn adjacent_requests_alternate_with_due_work_and_preserve_issuance_on_restart() {
    let (directory, mut app) = disposable();
    let path = directory.path().join("tactics.sqlite");
    let regular = seed_due(&mut app, "Due review");
    let other_regular = seed_due_with_answer(&mut app, "Other due review", "d2d4");
    assert_ne!(regular, other_regular);
    let (mut detection, mut jobs) = host("1", "miss");
    import(&mut app, "Two decisions", "1. Kg2 Kg7 2. Kg1 Kh8");
    let report = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
    assert_eq!(report["statistics"]["decisions_scanned"], 2);
    assert_eq!(report["statistics"]["pending_requests"], 2);
    assert_eq!(report["observations"][0]["ply"], 2);
    assert_eq!(report["observations"][1]["ply"], 0);

    let dataset = app.game_dataset().unwrap();
    let requested = app.store.pending_tactical_requests(&dataset).unwrap();
    let first = app.next_exercise().unwrap().unwrap();
    assert_eq!(first.attempt.target, requested[0].target);
    let issued = app
        .store
        .stored_attempt(&first.attempt.id)
        .unwrap()
        .unwrap();
    assert_eq!(issued.opponent_lead_in.unwrap().uci, "h8g7");
    let after_first = app.snapshot().unwrap();
    drop(app);
    let mut app = local_runtime::open(&path).unwrap();
    assert_eq!(app.next_exercise().unwrap().unwrap().attempt, first.attempt);
    assert_eq!(app.snapshot().unwrap(), after_first);
    assert_eq!(
        app.store.pending_tactical_requests(&dataset).unwrap(),
        vec![requested[1].clone()]
    );
    assert!(
        app.store
            .has_pending_tactical_request_for_attempt(&dataset, &first.attempt.id)
            .unwrap()
    );
    assert!(app.store.regular_review_owed(&dataset).unwrap());
    let first_answer = answer(&mut app, &first, "a1a7", "first-miss");

    let review = app.next_exercise().unwrap().unwrap();
    let review_answer = if review.attempt.target == regular {
        "e2e4"
    } else {
        assert_eq!(review.attempt.target, other_regular);
        "d2d4"
    };
    let after_review = app.snapshot().unwrap();
    drop(app);
    let mut app = local_runtime::open(&path).unwrap();
    assert_eq!(
        app.next_exercise().unwrap().unwrap().attempt,
        review.attempt
    );
    assert_eq!(app.snapshot().unwrap(), after_review);
    assert!(!app.store.regular_review_owed(&dataset).unwrap());
    assert_eq!(
        app.store.pending_tactical_requests(&dataset).unwrap(),
        vec![requested[1].clone()]
    );
    assert!(
        !app.store
            .has_pending_tactical_request_for_attempt(&dataset, &first.attempt.id)
            .unwrap()
    );
    assert!(app.submit(first_answer).unwrap().assessment.is_some());
    assert_eq!(app.snapshot().unwrap(), after_review);
    answer(&mut app, &review, review_answer, "due-review");

    // Another ordinary due review is still available. The second request wins
    // because Queue's first regular issuance cleared the persisted turn.
    let second = app.next_exercise().unwrap().unwrap();
    assert_eq!(second.attempt.target, requested[1].target);
    let issued = app
        .store
        .stored_attempt(&second.attempt.id)
        .unwrap()
        .unwrap();
    assert!(issued.opponent_lead_in.is_none());
    assert!(
        app.store
            .has_pending_tactical_request_for_attempt(&dataset, &second.attempt.id)
            .unwrap()
    );
    assert!(
        app.store
            .pending_tactical_requests(&dataset)
            .unwrap()
            .is_empty()
    );
    answer(&mut app, &second, "a1a7", "second-miss");
    let spare = app.next_exercise().unwrap().unwrap();
    let spare_target = if review_answer == "e2e4" {
        other_regular
    } else {
        regular
    };
    assert_eq!(spare.attempt.target, spare_target);
}

#[test]
#[ignore = "requires TEMPO_TACTICAL_PYTHON and TEMPO_STOCKFISH"]
fn real_stockfish_position_reaches_study_with_source_move() {
    let (directory, mut app) = disposable();
    let path = directory.path().join("tactics.sqlite");
    let store = tempo_store::SqliteStore::open(&path).unwrap();
    let mut registry = DetectorRegistry::default();
    registry
        .register(
            HangingMaterialDetector::new(
                crate::tactical_engine::StockfishEvaluator::discover(&path).unwrap(),
                HangingMaterialConfig::default(),
            )
            .unwrap(),
        )
        .unwrap();
    let mut detection = GameDetection::new(
        registry,
        AnalysisOptions {
            deadline: Duration::from_secs(90),
            ..AnalysisOptions::default()
        },
    )
    .unwrap();
    let mut jobs = TacticalJobs::new(&detection, &["hanging-material"]);
    jobs.force_idle_for_test();
    let dataset = app.game_dataset().unwrap();
    app.import_game_pgn(GameImport {
        dataset_id: dataset,
        command_id: "from-real-stockfish".into(),
        learner: "Learner".into(),
        repertoire_id: None,
        pgn: "[White \"Learner\"]\n[Black \"Opponent\"]\n[SetUp \"1\"]\n[FEN \"r5k1/pp2qppp/8/8/3P4/8/PP3PPP/R2Q2K1 b - - 0 1\"]\n\n1... Qe5 2. h3 0-1\n".into(),
    }).unwrap();
    let started = Instant::now();
    let mut study_read = Duration::ZERO;
    let mut task_poll = Duration::ZERO;
    let result = loop {
        detection.poll(&app);
        let tick = Instant::now();
        jobs.poll(&mut app, &store, &mut detection);
        task_poll = task_poll.max(tick.elapsed());
        let tick = Instant::now();
        app.study_progress(None).unwrap();
        study_read = study_read.max(tick.elapsed());
        let result = read(&app, &jobs, &store);
        if result["statistics"]["pending_games"] == 0 {
            break result;
        }
        assert!(started.elapsed() < Duration::from_secs(85), "{result}");
        std::thread::sleep(Duration::from_millis(15));
    };
    assert_eq!(result["statistics"]["decisions_scanned"], 1);
    assert_eq!(result["statistics"]["outcomes"]["missed_good_capture"], 1);
    assert_eq!(
        result["observations"][0]["details"]["assessment"]["puzzle"]["answer"]["uci"],
        "d4e5"
    );
    if let Some(destination) = std::env::var_os("TEMPO_TEST_POSITION_DATABASE") {
        std::fs::copy(&path, destination).unwrap();
    }
    let issued = app.next_exercise().unwrap().unwrap();
    let record = app
        .store
        .stored_attempt(&issued.attempt.id)
        .unwrap()
        .unwrap();
    assert_eq!(record.opponent_lead_in.unwrap().uci, "e7e5");
    answer(&mut app, &issued, "d4e5", "completed-real-puzzle");
    println!(
        "Elapsed {:?}, slowest task tick {:?}, slowest Study progress read {:?}",
        started.elapsed(),
        task_poll,
        study_read
    );
}

#[test]
fn admitted_detector_set_persists_each_producer_through_the_durable_worker() {
    use tempo_chess::{DetectionOutcome, GameDetectionInput, GameDetector, PositionPass};

    struct Second(HangingMaterialDetector<Evaluator>);
    impl GameDetector for Second {
        fn descriptor(&self) -> DetectorDescriptor {
            let mut descriptor = self.0.descriptor();
            descriptor.id = "test-second-analysis".into();
            descriptor
        }
        fn produces_tactical_assessments(&self) -> bool {
            true
        }
        fn detect(
            &self,
            input: &GameDetectionInput,
            control: &dyn DetectionControl,
        ) -> DetectionOutcome {
            self.0.detect(input, control)
        }
        fn detect_position(
            &self,
            position: &PositionPass<'_>,
            control: &dyn DetectionControl,
        ) -> DetectionOutcome {
            self.0.detect_position(position, control)
        }
    }
    let (directory, mut app) = disposable();
    let engine = || {
        HangingMaterialDetector::new(
            Evaluator {
                revision: "1",
                outcome: "capture",
            },
            HangingMaterialConfig::default(),
        )
        .unwrap()
    };
    let mut registry = DetectorRegistry::default();
    registry.register(engine()).unwrap();
    registry.register(Second(engine())).unwrap();
    let mut detection = GameDetection::new(registry, AnalysisOptions::default()).unwrap();
    let mut jobs = TacticalJobs::new(&detection, &["hanging-material", "test-second-analysis"]);
    import(&mut app, "Two producers", "1. Rxa7 Kg8");
    let report = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
    assert_eq!(report["statistics"]["decisions_scanned"], 1);
    assert_eq!(report["statistics"]["outcomes"]["took_good_capture"], 2);
    let ids: std::collections::BTreeSet<_> = report["observations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["producer"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["hanging-material", "test-second-analysis"]
            .into_iter()
            .collect()
    );
    drop(app);
    let path = directory.path().join("tactics.sqlite");
    let app = local_runtime::open(&path).unwrap();
    let store = tempo_store::SqliteStore::open(&path).unwrap();
    let reversed = TacticalJobs::new(&detection, &["test-second-analysis", "hanging-material"]);
    let replay = read(&app, &reversed, &store);
    assert_eq!(replay["analysis_version"], report["analysis_version"]);
    assert_eq!(replay["observations"], report["observations"]);
    assert_eq!(replay["statistics"]["pending_games"], 0);
}

#[test]
fn independent_mate_finding_needs_no_hanging_capture_and_skips_only_later_members() {
    use tempo_chess::{
        ChessAnswer, DetectedMoment, DetectionOutcome, GameDetectionInput, GameDetector,
        PositionPass, TacticalAssessment, TacticalKind, TacticalOutcome,
    };
    struct MateEngine;
    impl TacticalEvaluator for MateEngine {
        fn descriptor(&self) -> EvaluationProvenance {
            EvaluationProvenance {
                engine: "Mate fixture".into(),
                engine_version: "1".into(),
                configuration_fingerprint: "fixture".into(),
            }
        }
        fn evaluate(
            &self,
            request: &TacticalEvaluationRequest,
            _: &dyn DetectionControl,
        ) -> Result<RootEvaluation, EvaluationFailure> {
            Ok(RootEvaluation {
                provenance: self.descriptor(),
                depth: 12,
                nodes: 10_000,
                roots: request
                    .legal_moves
                    .iter()
                    .map(|uci| RootScore {
                        uci: uci.clone(),
                        bound: ScoreBound::Exact,
                        score: if uci == "f7f8" {
                            EvaluationScore::Mate(1)
                        } else {
                            EvaluationScore::Centipawns(0)
                        },
                    })
                    .collect(),
            })
        }
    }
    struct MateCheck(&'static str);
    impl GameDetector for MateCheck {
        fn descriptor(&self) -> DetectorDescriptor {
            DetectorDescriptor {
                id: self.0.into(),
                revision: "1".into(),
                label: "Mate test".into(),
                configuration_fingerprint: "fixture".into(),
            }
        }
        fn produces_tactical_assessments(&self) -> bool {
            true
        }
        fn detect(&self, _: &GameDetectionInput, _: &dyn DetectionControl) -> DetectionOutcome {
            DetectionOutcome::Unsupported
        }
        fn detect_position(
            &self,
            position: &PositionPass<'_>,
            control: &dyn DetectionControl,
        ) -> DetectionOutcome {
            assert_eq!(
                self.0, "mate-analysis",
                "a later detector ran after the mate"
            );
            assert!(position.move_is_mate("f7f8"));
            let assessment = TacticalAssessment {
                kind: TacticalKind::WinningMate,
                candidates: vec![],
                played: position.occurrence().next_move().unwrap().clone(),
                outcome: TacticalOutcome::WinningMate,
                analysis: position.compare(&MateEngine, control).unwrap(),
                standalone_analysis: None,
                accepted_moves: vec![ChessAnswer::from("f7f8")],
                puzzle: None,
                equivalence_cp: 0,
                personal: None,
            };
            DetectionOutcome::Complete(vec![DetectedMoment {
                occurrence: position.occurrence().offset(),
                code: "winning_mate".into(),
                subjects: vec![],
                candidate_move: Some("f7f8".into()),
                explanation: "Winning mate.".into(),
                tactical: Some(assessment),
            }])
        }
    }
    let (directory, mut app) = disposable();
    let mut registry = DetectorRegistry::default();
    registry
        .register(
            HangingMaterialDetector::new(
                Evaluator {
                    revision: "1",
                    outcome: "capture",
                },
                HangingMaterialConfig::default(),
            )
            .unwrap(),
        )
        .unwrap();
    registry.register(MateCheck("mate-analysis")).unwrap();
    registry.register(MateCheck("z-unvisited")).unwrap();
    let mut detection = GameDetection::new(registry, AnalysisOptions::default()).unwrap();
    let mut jobs = TacticalJobs::new(
        &detection,
        &["z-unvisited", "mate-analysis", "hanging-material"],
    );
    let fen = "7k/5Q2/6K1/8/8/8/8/8 w - - 0 1";
    assert!(tempo_chess::hanging_captures(fen).unwrap().is_empty());
    let played =
        tempo_chess::parse_played_games(&format!("[SetUp \"1\"]\n[FEN \"{fen}\"]\n\n1. Qf8# 1-0"))
            .unwrap()
            .pop()
            .unwrap()
            .unwrap();
    let input = GameDetectionInput::new(played.timeline(), tempo_chess::LearnerColor::White);
    struct Running;
    impl DetectionControl for Running {
        fn check(&self) -> Result<(), tempo_chess::DetectionStop> {
            Ok(())
        }
    }
    let control = Running;
    let producer = MateCheck("mate-analysis");
    let DetectionOutcome::Complete(mut claims) =
        tempo_chess::scan_position(&input, 0, [&producer as &dyn GameDetector], &control)
    else {
        panic!("mate fixture should complete");
    };
    let mut claim = claims.pop().unwrap().moment;
    assert!(tempo_chess::validate_detected_moments(&input, vec![claim.clone()]).is_ok());
    claim.tactical.as_mut().unwrap().personal = Some(tempo_chess::PersonalTacticsEvidence {
        candidates: vec![],
        puzzle: None,
    });
    assert!(tempo_chess::validate_detected_moments(&input, vec![claim]).is_err());
    let dataset_id = app.game_dataset().unwrap();
    app.import_game_pgn(GameImport {
        dataset_id, command_id: "mate-without-capture".into(), learner: "Learner".into(), repertoire_id: None,
        pgn: format!("[White \"Learner\"]\n[Black \"Opponent\"]\n[SetUp \"1\"]\n[FEN \"{fen}\"]\n\n1. Qf8# 1-0\n"),
    }).unwrap();
    let result = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
    assert_eq!(result["statistics"]["observations"], 1);
    let finding = &result["observations"][0];
    assert_eq!(finding["producer"]["id"], "mate-analysis");
    assert_eq!(finding["outcome"], "winning_mate");
    assert_eq!(
        finding["details"]["skipped_analyses"],
        serde_json::json!(["z-unvisited"])
    );
    assert!(finding["puzzle"].is_null());
}

#[test]
fn adding_an_earlier_detector_preserves_the_fulfilled_occurrence() {
    use tempo_chess::{DetectionOutcome, GameDetectionInput, GameDetector, PositionPass};
    struct EmptyCheck;
    impl GameDetector for EmptyCheck {
        fn descriptor(&self) -> DetectorDescriptor {
            DetectorDescriptor {
                id: "earlier-analysis".into(),
                revision: "1".into(),
                label: "Earlier analysis".into(),
                configuration_fingerprint: "fixture".into(),
            }
        }
        fn produces_tactical_assessments(&self) -> bool {
            true
        }
        fn detect(&self, _: &GameDetectionInput, _: &dyn DetectionControl) -> DetectionOutcome {
            DetectionOutcome::Unsupported
        }
        fn detect_position(
            &self,
            _: &PositionPass<'_>,
            _: &dyn DetectionControl,
        ) -> DetectionOutcome {
            DetectionOutcome::Complete(vec![])
        }
    }

    let (directory, mut app) = disposable();
    let (mut detection, mut jobs) = host("1", "miss");
    import(&mut app, "Stable finding", "1. Kg2 Kg7");
    let initial = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
    let issued = app.next_exercise().unwrap().unwrap();
    answer(&mut app, &issued, "a1a7", "settle-before-set-change");
    let original = app.snapshot().unwrap();
    drop(jobs);
    drop(detection);

    let mut registry = DetectorRegistry::default();
    registry.register(EmptyCheck).unwrap();
    registry
        .register(
            HangingMaterialDetector::new(
                Evaluator {
                    revision: "1",
                    outcome: "miss",
                },
                HangingMaterialConfig::default(),
            )
            .unwrap(),
        )
        .unwrap();
    let mut detection = GameDetection::new(registry, AnalysisOptions::default()).unwrap();
    let mut jobs = TacticalJobs::new(&detection, &["earlier-analysis", "hanging-material"]);
    let revised = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
    assert_ne!(revised["analysis_version"], initial["analysis_version"]);
    assert_eq!(
        revised["observations"][0]["id"],
        initial["observations"][0]["id"]
    );
    assert_eq!(
        revised["observations"][0]["request_id"],
        initial["observations"][0]["request_id"]
    );
    assert_eq!(revised["observations"][0]["fulfilled"], true);
    assert_eq!(revised["statistics"]["pending_requests"], 0);
    let after = app.snapshot().unwrap();
    assert_eq!(after.learning, original.learning);
    assert_eq!(after.assessments, original.assessments);
}

#[test]
fn personal_findings_flow_through_games_and_study_and_reuse_the_previous_request() {
    use tempo_chess::{
        DetectionOutcome, GameDetectionInput, GameDetector, PositionPass, TacticalEvidenceFormat,
    };
    struct Renamed(tempo_chess::PersonalTacticsDetector<Evaluator>);
    impl GameDetector for Renamed {
        fn descriptor(&self) -> DetectorDescriptor {
            DetectorDescriptor {
                id: "z-archive".into(),
                label: "Display name changed".into(),
                ..self.0.descriptor()
            }
        }
        fn tactical_evidence_format(&self) -> Option<TacticalEvidenceFormat> {
            self.0.tactical_evidence_format()
        }
        fn detect(
            &self,
            input: &GameDetectionInput,
            control: &dyn DetectionControl,
        ) -> DetectionOutcome {
            self.0.detect(input, control)
        }
        fn detect_position(
            &self,
            position: &PositionPass<'_>,
            control: &dyn DetectionControl,
        ) -> DetectionOutcome {
            self.0.detect_position(position, control)
        }
    }
    struct Earlier;
    impl GameDetector for Earlier {
        fn descriptor(&self) -> DetectorDescriptor {
            DetectorDescriptor {
                id: "a-earlier".into(),
                label: "Secondary".into(),
                revision: "1".into(),
                configuration_fingerprint: "fixture".into(),
            }
        }
        fn produces_tactical_assessments(&self) -> bool {
            true
        }
        fn detect(&self, _: &GameDetectionInput, _: &dyn DetectionControl) -> DetectionOutcome {
            DetectionOutcome::Complete(vec![])
        }
        fn detect_position(
            &self,
            _: &PositionPass<'_>,
            _: &dyn DetectionControl,
        ) -> DetectionOutcome {
            DetectionOutcome::Complete(vec![])
        }
    }
    let (directory, mut app) = disposable();
    let store = tempo_store::SqliteStore::open(directory.path().join("tactics.sqlite")).unwrap();
    let dataset = app.game_dataset().unwrap();
    let sample = |name, control, line| {
        format!(
            "[White \"Learner\"]\n[Black \"{name}\"]\n[WhiteElo \"1499\"]\n[BlackElo \"1500\"]\n[TimeControl \"{control}\"]\n[SetUp \"1\"]\n[FEN \"{HANGING}\"]\n\n1. {line}"
        )
    };
    let pgn = format!(
        "{}\n\n{}",
        sample("Missed", "60", "Kg2 0-1"),
        sample("Played", "60+0", "Rxa7 1-0")
    );
    assert_eq!(
        app.import_game_pgn(GameImport {
            dataset_id: dataset.clone(),
            command_id: "personal-two".into(),
            learner: "Learner".into(),
            repertoire_id: None,
            pgn
        })
        .unwrap()
        .new_games,
        2
    );
    let (mut old_detector, mut old_jobs) = host("legacy", "good");
    scan(&directory, &mut app, &mut old_detector, &mut old_jobs, 2);
    let original_requests = store.pending_tactical_requests(&dataset).unwrap();
    let [previous] = original_requests.as_slice() else {
        panic!("expected one legacy request")
    };
    let mut registry = DetectorRegistry::default();
    registry.register(Earlier).unwrap();
    registry
        .register(Renamed(tempo_chess::PersonalTacticsDetector(Evaluator {
            revision: "personal",
            outcome: "good",
        })))
        .unwrap();
    let duplicate = registry
        .register(tempo_chess::PersonalTacticsDetector(Evaluator {
            revision: "competing",
            outcome: "good",
        }))
        .unwrap_err();
    assert_eq!(duplicate.code, "duplicate_complete_decision_detector");
    let mut detector = GameDetection::new(
        registry,
        AnalysisOptions {
            deadline: Duration::from_secs(5),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(detector.complete_decision_detector(), Some("z-archive"));
    let mut jobs = TacticalJobs::new(&detector, &["a-earlier", "z-archive"]);
    assert!(jobs.has_personal_report());
    scan(&directory, &mut app, &mut detector, &mut jobs, 2);
    let requests = store.pending_tactical_requests(&dataset).unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].id, previous.id);
    assert_eq!(requests[0].target, previous.target);
    let query = json!({"dataset_id": dataset, "filters": {"clock":{"seconds":60,"increment":0},
        "speed":"bullet","learner_rating":1450,"motif":"hanging_material"}});
    let report = crate::personal_tactics_report::read(
        &store,
        jobs.report_version(),
        serde_json::from_value(query).unwrap(),
    )
    .unwrap();
    assert_eq!(report["filtered_games"], 2);
    assert_eq!(report["clocks"].as_array().unwrap().len(), 1);
    assert_eq!(report["loss_comparison"]["with_miss"]["losses"], 1);
    assert_eq!(report["loss_comparison"]["without_miss"]["wins"], 1);
    assert_eq!(report["games"].as_array().unwrap().len(), 2);
    let active = app.next_exercise().unwrap().unwrap();
    assert_eq!(active.attempt.target, previous.target);
    let saved = answer(&mut app, &active, "a1a7", "personal-answer");
    drop(app);
    let mut reopened = local_runtime::open(&directory.path().join("tactics.sqlite")).unwrap();
    assert_eq!(
        reopened.submit(saved).unwrap().attempt.status,
        tempo_study::AttemptStatus::Complete
    );
    assert!(
        store
            .pending_tactical_requests(&dataset)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_revised_tactic_preserves_the_issued_attempt_without_introducing_new_learning() {
    let (directory, mut app) = disposable();
    let store = tempo_store::SqliteStore::open(directory.path().join("tactics.sqlite")).unwrap();
    let dataset = app.game_dataset().unwrap();
    app.import_game_pgn(GameImport { dataset_id: dataset.clone(), command_id: "rescan".into(), learner: "Learner".into(), repertoire_id: None,
        pgn: "[White \"Learner\"]\n[Black \"Opponent\"]\n[SetUp \"1\"]\n[FEN \"7k/n7/8/4q3/3P4/8/8/R5K1 w - - 0 1\"]\n\n1. Kg2 0-1".into(),
    }).unwrap();
    let (mut old_detector, mut old_jobs) = host("before", "good");
    scan(&directory, &mut app, &mut old_detector, &mut old_jobs, 1);
    let original = app
        .store
        .pending_tactical_requests(&dataset)
        .unwrap()
        .remove(0);
    let before = app.store.content_state().unwrap();
    let mut registry = DetectorRegistry::default();
    registry
        .register(tempo_chess::PersonalTacticsDetector(Evaluator {
            revision: "revised-before-issuance",
            outcome: "different",
        }))
        .unwrap();
    let mut early_detector = GameDetection::new(registry, AnalysisOptions::default()).unwrap();
    let mut early_jobs = TacticalJobs::new(&early_detector, &["personal-tactics"]);
    scan(
        &directory,
        &mut app,
        &mut early_detector,
        &mut early_jobs,
        1,
    );
    assert_eq!(
        app.store.pending_tactical_requests(&dataset).unwrap(),
        vec![original.clone()]
    );
    assert_eq!(app.store.content_state().unwrap().catalog, before.catalog);
    assert_eq!(app.store.content_state().unwrap().learning, before.learning);
    let issued = app.next_exercise().unwrap().unwrap();
    assert_eq!(issued.attempt.target, original.target);
    assert_eq!(issued.attempt.content_version, original.version);
    let prior = app.store.content_state().unwrap();
    let mut registry = DetectorRegistry::default();
    registry
        .register(tempo_chess::PersonalTacticsDetector(Evaluator {
            revision: "revised",
            outcome: "different",
        }))
        .unwrap();
    let mut detector = GameDetection::new(
        registry,
        AnalysisOptions {
            deadline: Duration::from_secs(5),
            ..Default::default()
        },
    )
    .unwrap();
    let mut jobs = TacticalJobs::new(&detector, &["personal-tactics"]);
    scan(&directory, &mut app, &mut detector, &mut jobs, 1);
    let after = app.store.content_state().unwrap();
    assert_eq!(after.catalog, prior.catalog);
    assert_eq!(after.learning, prior.learning);
    assert!(
        store
            .has_pending_tactical_request_for_attempt(&dataset, &issued.attempt.id)
            .unwrap()
    );
    assert_eq!(
        app.next_exercise().unwrap().unwrap().attempt,
        issued.attempt
    );
    let report_request = || serde_json::from_value(json!({"dataset_id":dataset})).unwrap();
    let before_skip =
        crate::personal_tactics_report::read(&store, jobs.report_version(), report_request())
            .unwrap();
    assert_eq!(before_skip["detail"]["training"]["status"], "superseded");
    app.skip(crate::AbandonCommand {
        dataset_id: dataset.clone(),
        attempt_id: issued.attempt.id,
        expected_revision: issued.attempt.revision,
        submission_id: "skip-superseded".into(),
    })
    .unwrap();
    let after_skip =
        crate::personal_tactics_report::read(&store, jobs.report_version(), report_request())
            .unwrap();
    assert!(after_skip["detail"]["training"].is_null());
    assert!(
        store
            .pending_tactical_requests(&dataset)
            .unwrap()
            .is_empty()
    );
}

const TWO_CAPTURES: &str = "k7/3n4/8/8/3Q3r/8/8/1K6 w - - 0 1";

struct ChoiceEvaluator {
    revision: &'static str,
    best: &'static str,
}

impl TacticalEvaluator for ChoiceEvaluator {
    fn descriptor(&self) -> EvaluationProvenance {
        EvaluationProvenance {
            engine: "Consumer fixture".into(),
            engine_version: self.revision.into(),
            configuration_fingerprint: self.best.into(),
        }
    }

    fn evaluate(
        &self,
        request: &TacticalEvaluationRequest,
        _: &dyn DetectionControl,
    ) -> Result<RootEvaluation, EvaluationFailure> {
        Ok(RootEvaluation {
            provenance: self.descriptor(),
            depth: 12,
            nodes: 50_000,
            roots: request
                .legal_moves
                .iter()
                .map(|uci| RootScore {
                    uci: uci.clone(),
                    bound: ScoreBound::Exact,
                    score: EvaluationScore::Centipawns(if uci == self.best { 1000 } else { 0 }),
                })
                .collect(),
        })
    }
}

fn choice_host(revision: &'static str, best: &'static str) -> (GameDetection, TacticalJobs) {
    let mut registry = DetectorRegistry::default();
    registry
        .register(
            HangingMaterialDetector::new(
                ChoiceEvaluator { revision, best },
                HangingMaterialConfig::default(),
            )
            .unwrap(),
        )
        .unwrap();
    let detection = GameDetection::new(
        registry,
        AnalysisOptions {
            deadline: Duration::from_secs(5),
            retained_results: 8,
        },
    )
    .unwrap();
    let jobs = TacticalJobs::new(&detection, &["hanging-material"]);
    (detection, jobs)
}

fn import_two_captures(app: &mut Tempo) {
    app.import_game_pgn(GameImport {
        dataset_id: app.game_dataset().unwrap(),
        command_id: "rescan-two-captures".into(),
        learner: "Learner".into(),
        repertoire_id: None,
        pgn: format!(
            "[White \"Learner\"]\n[Black \"Opponent\"]\n[SetUp \"1\"]\n[FEN \"{TWO_CAPTURES}\"]\n[Result \"0-1\"]\n\n1. Kc1 0-1"
        ),
    })
    .unwrap();
}

#[test]
fn revised_issued_or_fulfilled_finding_does_not_introduce_an_alternate_due_puzzle() {
    for already_fulfilled in [false, true] {
        let (directory, mut app) = disposable();
        import_two_captures(&mut app);
        let (mut detection, mut jobs) = choice_host("v1", "d4d7");
        let original_report = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
        let issued = app.next_exercise().unwrap().unwrap();
        let answer_command = if already_fulfilled {
            let saved = answer(&mut app, &issued, "d4d7", "fulfilled-before-rescan");
            assert!(app.next_exercise().unwrap().is_none());
            Some(saved)
        } else {
            None
        };
        let before = app.snapshot().unwrap();

        let (mut detection, mut jobs) = choice_host("v2", "d4h4");
        let revised = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
        let old_row = &original_report["observations"][0];
        let new_row = &revised["observations"][0];
        assert_eq!(new_row["request_id"], old_row["request_id"]);
        assert_eq!(new_row["puzzle"], old_row["puzzle"]);
        assert_eq!(new_row["fulfilled"], already_fulfilled);
        assert_eq!(
            new_row["details"]["assessment"]["puzzle"]["answer"]["uci"],
            "d4h4"
        );
        let after = app.snapshot().unwrap();
        assert_eq!(after.catalog, before.catalog);
        assert_eq!(after.content_history, before.content_history);
        assert_eq!(after.attempts, before.attempts);
        assert_eq!(after.active_attempt, before.active_attempt);
        assert_eq!(after.learning, before.learning);
        assert_eq!(after.assessments, before.assessments);

        let fixed_time = app.components.clock.now();
        drop(app);
        let mut app = local_runtime::open(&directory.path().join("tactics.sqlite")).unwrap();
        app.components.clock = Box::new(FixedClock(fixed_time));
        if let Some(saved) = answer_command {
            assert_eq!(
                app.submit(saved).unwrap().attempt.status,
                tempo_study::AttemptStatus::Complete
            );
            assert_eq!(app.snapshot().unwrap().assessments, before.assessments);
        } else {
            let resumed = app.next_exercise().unwrap().unwrap();
            assert_eq!(resumed.attempt, issued.attempt);
            answer(&mut app, &resumed, "d4d7", "fulfilled-after-rescan");
        }
        assert!(app.next_exercise().unwrap().is_none());
    }
}

#[test]
fn revised_unissued_finding_reports_new_answer_and_serves_retained_puzzle() {
    let (directory, mut app) = disposable();
    import_two_captures(&mut app);
    let (mut detection, mut jobs) = choice_host("v1", "d4d7");
    let original = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
    let dataset = app.game_dataset().unwrap();
    let original_request = app
        .store
        .pending_tactical_requests(&dataset)
        .unwrap()
        .remove(0);
    let before = app.snapshot().unwrap();

    let (mut detection, mut jobs) = choice_host("v2", "d4h4");
    let revised = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
    assert_eq!(
        revised["observations"][0]["request_id"],
        original["observations"][0]["request_id"]
    );
    assert_eq!(
        revised["observations"][0]["puzzle"],
        original["observations"][0]["puzzle"]
    );
    assert_eq!(
        revised["observations"][0]["details"]["assessment"]["puzzle"]["answer"]["uci"],
        "d4h4"
    );
    assert_eq!(
        app.store.pending_tactical_requests(&dataset).unwrap(),
        vec![original_request.clone()]
    );
    let revised_snapshot = app.snapshot().unwrap();
    assert_eq!(revised_snapshot.catalog, before.catalog);
    assert_eq!(revised_snapshot.learning, before.learning);
    assert_eq!(revised_snapshot.content_history, before.content_history);

    let time = app.components.clock.now();
    drop(app);
    let mut reopened = local_runtime::open(&directory.path().join("tactics.sqlite")).unwrap();
    reopened.components.clock = Box::new(FixedClock(time));
    let next = reopened.next_exercise().unwrap().unwrap();
    assert_eq!(next.attempt.target, original_request.target);
    assert_eq!(next.attempt.content_version, original_request.version);
    answer(&mut reopened, &next, "d4d7", "retained-answer");
    assert!(reopened.next_exercise().unwrap().is_none());
}

#[test]
fn cancelled_or_abandoned_finding_keeps_its_binding_without_creating_alternate_work() {
    for abandon in [false, true] {
        let (directory, mut app) = disposable();
        import_two_captures(&mut app);
        let (mut detection, mut jobs) = choice_host("original", "d4d7");
        scan(&directory, &mut app, &mut detection, &mut jobs, 1);
        let dataset = app.game_dataset().unwrap();
        let request = app
            .store
            .pending_tactical_requests(&dataset)
            .unwrap()
            .remove(0);
        let before = app.snapshot().unwrap();
        let skip = if abandon {
            let issue = app.next_exercise().unwrap().unwrap();
            let command = crate::AbandonCommand {
                dataset_id: dataset.clone(),
                attempt_id: issue.attempt.id,
                expected_revision: issue.attempt.revision,
                submission_id: "skip-original-before-rescan".into(),
            };
            let reply = app.skip(command.clone()).unwrap();
            Some((command, reply))
        } else {
            let (mut detection, mut jobs) = choice_host("invalidated", "b1c1");
            let invalidated = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
            assert_eq!(invalidated["statistics"]["pending_requests"], 0);
            let cancelled = app.snapshot().unwrap();
            assert_eq!(cancelled.catalog, before.catalog);
            assert_eq!(cancelled.learning, before.learning);
            None
        };

        let (mut detection, mut jobs) = choice_host("revised", "d4h4");
        let revised = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
        assert_eq!(
            revised["observations"][0]["details"]["assessment"]["puzzle"]["answer"]["uci"],
            "d4h4"
        );
        let after = app.snapshot().unwrap();
        assert_eq!(after.catalog, before.catalog);
        assert_eq!(after.learning, before.learning);
        assert_eq!(after.assessments, before.assessments);
        let pending = app.store.pending_tactical_requests(&dataset).unwrap();
        if let Some((command, reply)) = skip {
            assert_eq!(pending, vec![request]);
            assert_eq!(app.skip(command).unwrap(), reply);
            assert!(app.next_exercise().unwrap().is_none());
        } else {
            assert!(pending.is_empty());
            let due = app.next_exercise().unwrap().unwrap();
            assert_eq!(due.attempt.target, request.target);
            assert!(
                !app.store
                    .has_pending_tactical_request_for_attempt(&dataset, &due.attempt.id)
                    .unwrap()
            );
            answer(&mut app, &due, "d4d7", "retained-ordinary-due");
            assert!(app.next_exercise().unwrap().is_none());
        }
    }
}

#[test]
fn rescan_reports_a_pinned_inactive_puzzle_without_reactivation_or_retargeting() {
    let (directory, mut app) = disposable();
    import_two_captures(&mut app);
    let (mut detection, mut jobs) = choice_host("original", "d4d7");
    scan(&directory, &mut app, &mut detection, &mut jobs, 1);
    let before = app.snapshot().unwrap();
    let target = before.catalog.items.keys().next().unwrap().clone();
    let mut catalog = before.catalog;
    catalog.items.get_mut(&target).unwrap().status = tempo_chess::ContentStatus::Retired;
    app.store
        .commit(Commit {
            expected_dataset: before.dataset,
            expected_revision: before.revision,
            writes: vec![Write::Catalog(catalog.clone())],
        })
        .unwrap();
    for (revision, best) in [("unchanged-retired", "d4d7"), ("changed-retired", "d4h4")] {
        let (mut detection, mut jobs) = choice_host(revision, best);
        let report = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
        assert_eq!(
            report["observations"][0]["details"]["assessment"]["puzzle"]["answer"]["uci"],
            best
        );
        assert_eq!(app.snapshot().unwrap().catalog, catalog);
        assert_eq!(app.snapshot().unwrap().learning, before.learning);
        assert!(app.next_exercise().unwrap().is_none());
    }
    let version = catalog.items.get_mut(&target).unwrap();
    version.status = tempo_chess::ContentStatus::Active;
    version.version = tempo_types::ContentVersion(version.version.0 + 1);
    let snapshot = app.snapshot().unwrap();
    let dataset = snapshot.dataset.clone();
    app.store
        .commit(Commit {
            expected_dataset: dataset.clone(),
            expected_revision: snapshot.revision,
            writes: vec![Write::Catalog(catalog.clone())],
        })
        .unwrap();
    let (mut detection, mut jobs) = choice_host("outdated-binding-version", "d4h4");
    scan(&directory, &mut app, &mut detection, &mut jobs, 1);
    assert!(
        app.store
            .pending_tactical_requests(&dataset)
            .unwrap()
            .is_empty()
    );
    let current_due = app.next_exercise().unwrap().unwrap();
    assert_eq!(current_due.attempt.target, target);
    assert_eq!(
        current_due.attempt.content_version,
        catalog.items[&target].version
    );
    assert!(
        !app.store
            .has_pending_tactical_request_for_attempt(&dataset, &current_due.attempt.id)
            .unwrap()
    );
}

#[test]
fn a_tactical_request_does_not_record_whether_it_introduced_prior_learning() {
    use crate::{JsonSnapshotCodec, SnapshotCodec};

    let (directory, mut source) = disposable();
    let fixed_time = source.components.clock.now();
    import_two_captures(&mut source);
    let (mut detector, mut jobs) = choice_host("v1", "d4d7");
    scan(&directory, &mut source, &mut detector, &mut jobs, 1);
    let codec = JsonSnapshotCodec;
    let mut captured = codec.decode(&source.export(&codec).unwrap()).unwrap();
    captured.game_evidence = Default::default();
    let target = captured.catalog.items.keys().next().unwrap().clone();
    assert!(captured.learning[&target].introduced);

    let snapshot_after_scan = |snapshot: &crate::PortableSnapshot| {
        let dataset: DatasetId = "restored-before-scan".into();
        let (directory, mut app) = disposable();
        app.components.clock = Box::new(FixedClock(fixed_time));
        app.restore(dataset.clone(), &codec.encode(snapshot).unwrap(), &codec)
            .unwrap();
        import_two_captures(&mut app);
        let (mut detector, mut jobs) = choice_host("v1", "d4d7");
        let report = scan(&directory, &mut app, &mut detector, &mut jobs, 1);
        let request = app
            .store
            .pending_tactical_requests(&dataset)
            .unwrap()
            .remove(0);
        let saved = app.snapshot().unwrap();
        assert!(saved.attempts.is_empty());
        assert!(saved.assessments.is_empty());
        let (mut detector, mut jobs) = choice_host("v2", "d4h4");
        scan(&directory, &mut app, &mut detector, &mut jobs, 1);
        let revised = app.snapshot().unwrap();
        assert_eq!(revised.catalog, saved.catalog);
        assert_eq!(revised.learning, saved.learning);
        (
            report,
            request,
            saved.catalog,
            saved.learning,
            saved.content_history,
        )
    };

    // A portable dataset may already hold valid, ungraded learning without
    // importing the original request's or attempt's identity.
    let mut unseen = captured.clone();
    unseen.learning.insert(target, Default::default());
    assert_eq!(
        snapshot_after_scan(&captured),
        snapshot_after_scan(&unseen),
        "the request and Learning state cannot distinguish a pre-existing ungraded introduction"
    );
}
