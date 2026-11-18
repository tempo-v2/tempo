//! Durable tactic import and issued-version board replay.
use serde_json::Value;
use tempo::{AbandonCommand, Components, SubmitCommand, Tempo, wire::StudyCommand};
use tempo_chess::{LegalSequenceGrader, TacticRecord, apply_setup_move, compile_tactics};
use tempo_store::SqliteStore;
use tempo_types::Grade;

use std::path::Path;

fn reopen(path: &Path) -> Tempo {
    let mut components = Components::fixtures();
    components.grader = Box::new(LegalSequenceGrader);
    components.learning = Box::new(tempo_learning::DailyLearning::default());
    Tempo::new(Box::new(SqliteStore::open(path).unwrap()), components).unwrap()
}

fn practice(path: &Path) -> Tempo {
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_tempo"))
        .arg("init-practice")
        .arg(path)
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    reopen(path)
}

fn white_mate_in_two() -> TacticRecord {
    // Tempo's mateIn2-focused-04, Lichess 03ySM (catalog source CC0).
    TacticRecord {
        namespace: "tempo-pack-test".into(),
        source_id: "03ySM".into(),
        solver_fen: apply_setup_move(
            "2r3k1/5pp1/Qq2b2p/3p4/3P4/3B3P/1P3PP1/4R1K1 b - - 0 26",
            "b6d4",
        )
        .unwrap(),
        solution: ["a6c8", "e6c8", "e1e8"].map(str::to_owned).to_vec(),
        label: "Mate in two".into(),
        provenance: vec!["mateIn2-focused-04".into()],
    }
}

fn import(app: &mut Tempo, row: &TacticRecord) {
    let catalog = compile_tactics(std::slice::from_ref(row)).unwrap();
    app.apply_catalog(&app.snapshot().unwrap().dataset, &catalog)
        .unwrap();
}

fn reply(app: &mut Tempo, command: StudyCommand) -> Value {
    serde_json::to_value(app.handle(command)).unwrap()
}

#[test]
fn actual_tactic_replays_issued_board_after_restart_and_a_semantic_edit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("practice.sqlite");
    let mut app = practice(&path);
    let mut tactic = white_mate_in_two();
    import(&mut app, &tactic);
    let issued = app.next_exercise().unwrap().unwrap();
    let first = SubmitCommand {
        dataset_id: issued.attempt.dataset.clone(),
        attempt_id: issued.attempt.id.clone(),
        expected_revision: issued.attempt.revision,
        submission_id: "first-decision".into(),
        answer: "a6c8".into(),
    };
    let before = app.snapshot().unwrap();
    let invalid = reply(
        &mut app,
        StudyCommand::Answer(SubmitCommand {
            answer: "a6a6".into(),
            ..first.clone()
        }),
    );
    assert_eq!(invalid["code"], "invalid_move");
    assert_eq!(invalid["retryable"], false);
    assert_eq!(app.snapshot().unwrap(), before);
    let middle = reply(&mut app, StudyCommand::Answer(first.clone()));
    assert_eq!(middle["status"], "ready");
    assert_eq!(middle["learner_color"], "white");
    assert_eq!(middle["display_fen"], middle["prompt"]["fen"]);
    assert_eq!(middle["feedback"]["autoplay"][0]["uci"], "e6c8");
    assert!(middle["assessment"].is_null());
    assert!(app.snapshot().unwrap().assessments.is_empty());

    let last = StudyCommand::Answer(SubmitCommand {
        expected_revision: tempo_types::Revision(1),
        submission_id: "finish-puzzle".into(),
        answer: "e1e8".into(),
        ..first
    });
    let completed = reply(&mut app, last.clone());
    assert_eq!(completed["status"], "complete");
    assert_eq!(completed["assessment"], "good");
    assert_eq!(completed["legal_moves"], serde_json::json!([]));
    assert_ne!(completed["display_fen"], middle["display_fen"]);
    let done = app.snapshot().unwrap();
    assert_eq!(done.assessments.len(), 1);
    import(&mut app, &tactic);
    assert_eq!(app.snapshot().unwrap(), done);
    tactic.label = "Renamed".into();
    import(&mut app, &tactic);
    assert_eq!(app.snapshot().unwrap().learning, done.learning);

    // A legal alternative changes the issued definition. The old final board
    // must still be reconstructible exclusively from its archived version.
    tactic.solution = vec!["a6b7".into()];
    import(&mut app, &tactic);
    drop(app);

    let mut restarted = reopen(&path);
    let edited = restarted.snapshot().unwrap();
    assert_eq!(edited.assessments, done.assessments);
    assert_eq!(edited.content_history.len(), 2);
    assert_eq!(reply(&mut restarted, last), completed);
    assert_eq!(restarted.snapshot().unwrap(), edited);
    let resume = reply(
        &mut restarted,
        StudyCommand::Resume {
            attempt_id: issued.attempt.id,
        },
    );
    assert_eq!(resume["code"], "stale_content");
}

#[test]
fn a_legal_miss_keeps_the_board_and_only_the_original_again_through_abandon() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = practice(&dir.path().join("practice.sqlite"));
    import(&mut app, &white_mate_in_two());
    let issued = app.next_exercise().unwrap().unwrap().attempt;
    let resume = StudyCommand::Resume {
        attempt_id: issued.id.clone(),
    };
    let ready = reply(&mut app, resume.clone());
    assert!(
        ready["legal_moves"]
            .as_array()
            .unwrap()
            .contains(&Value::from("a6b7"))
    );
    let missed_move = StudyCommand::Answer(SubmitCommand {
        dataset_id: issued.dataset.clone(),
        attempt_id: issued.id.clone(),
        expected_revision: issued.revision,
        submission_id: "legal-miss".into(),
        answer: "a6b7".into(),
    });
    let miss = reply(&mut app, missed_move.clone());
    assert_eq!(miss["display_fen"], ready["display_fen"]);
    assert_eq!(miss["assessment"], "again");
    assert_eq!(miss["feedback"]["correction_uci"], "a6c8");
    let abandoned = reply(
        &mut app,
        StudyCommand::Abandon(AbandonCommand {
            dataset_id: issued.dataset,
            attempt_id: issued.id,
            expected_revision: tempo_types::Revision(1),
            submission_id: "abandon-after-miss".into(),
        }),
    );
    assert_eq!(abandoned["status"], "abandoned");
    assert_eq!(abandoned["display_fen"], ready["display_fen"]);
    assert_eq!(abandoned["assessment"], "again");
    let reloaded = reply(&mut app, resume);
    assert_eq!(reloaded, abandoned);
    let saved = app.snapshot().unwrap();
    assert_eq!(saved.assessments.len(), 1);
    assert_eq!(saved.assessments[0].input.grade, Grade::Again);

    let mut edited = white_mate_in_two();
    edited.solution = vec!["a6b7".into()];
    import(&mut app, &edited);
    let before_replay = app.snapshot().unwrap();
    assert_eq!(reply(&mut app, missed_move), miss);
    assert_eq!(app.snapshot().unwrap(), before_replay);
}

#[test]
fn an_invalid_cli_batch_with_a_valid_record_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("practice.sqlite");
    let app = practice(&path);
    let before = app.snapshot().unwrap();
    let tactic = white_mate_in_two();
    let valid = serde_json::json!({
        "namespace": tactic.namespace,
        "source_id": tactic.source_id,
        "solver_fen": tactic.solver_fen,
        "solution": tactic.solution,
        "label": tactic.label,
        "provenance": tactic.provenance,
    });
    let mut illegal = valid.clone();
    illegal["source_id"] = "bad-source".into();
    illegal["solution"] = serde_json::json!(["a6a6"]);
    let transfer = dir.path().join("batch.json");
    for invalid in [valid.clone(), illegal] {
        std::fs::write(
            &transfer,
            serde_json::json!({
                "schema_version": 1,
                "puzzles": [valid, invalid],
            })
            .to_string(),
        )
        .unwrap();
        let rejected = std::process::Command::new(env!("CARGO_BIN_EXE_tempo"))
            .arg("import-tactics")
            .arg(&path)
            .arg(&transfer)
            .output()
            .unwrap();
        assert!(!rejected.status.success());
        assert!(rejected.stdout.is_empty());
        assert_eq!(app.snapshot().unwrap(), before);
    }
}
