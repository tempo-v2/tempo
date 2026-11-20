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
use tempo_store::{ActivityMode, Store};

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
                    score: EvaluationScore::Centipawns(if uci == "a1a7" {
                        if self.outcome == "inferior" {
                            -1000
                        } else {
                            1000
                        }
                    } else {
                        0
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
    let snapshot = app.store.content_state().unwrap();
    let catalog = compile_tactics(&[TacticRecord {
        namespace: "fixture".into(),
        source_id: "ordinary".into(),
        solver_fen: tempo_chess::START_FEN.into(),
        solution: vec!["e2e4".into()],
        label: label.into(),
        provenance: vec![],
    }])
    .unwrap();
    let target = catalog.items.keys().next().unwrap().clone();
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
fn adjacent_positions_commit_separately_and_replay_the_exact_predecessor() {
    let (directory, mut app) = disposable();
    let regular = seed_due(&mut app, "Due review");
    let (mut detection, mut jobs) = host("1", "miss");
    import(&mut app, "Two decisions", "1. Kg2 Kg7 2. Kg1 Kh8");
    let report = scan(&directory, &mut app, &mut detection, &mut jobs, 1);
    assert_eq!(report["statistics"]["decisions_scanned"], 2);
    assert_eq!(report["statistics"]["pending_requests"], 2);
    assert_eq!(report["observations"][0]["ply"], 2);
    assert_eq!(report["observations"][1]["ply"], 0);
    let first = app.next_exercise().unwrap().unwrap();
    let issued = app
        .store
        .stored_attempt(&first.attempt.id)
        .unwrap()
        .unwrap();
    assert_eq!(issued.opponent_lead_in.unwrap().uci, "h8g7");
    answer(&mut app, &first, "a1a7", "first-miss");
    drop(app);
    let mut app = local_runtime::open(&directory.path().join("tactics.sqlite")).unwrap();
    let review = app.next_exercise().unwrap().unwrap();
    assert_eq!(review.attempt.target, regular);
    answer(&mut app, &review, "e2e4", "due-review");
    let second = app.next_exercise().unwrap().unwrap();
    let issued = app
        .store
        .stored_attempt(&second.attempt.id)
        .unwrap()
        .unwrap();
    assert!(issued.opponent_lead_in.is_none());
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
    registry
        .register(tempo_chess::PersonalTacticsDetector(Evaluator {
            revision: "personal",
            outcome: "good",
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
