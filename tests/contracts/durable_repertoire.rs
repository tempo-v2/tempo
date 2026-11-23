//! Drive real PGN import and SQLite replay across two application processes.

use super::support::{command, daily_components, open};
use serde_json::{Value, json};
use std::{env, fs, path::Path, process::Command};
use tempo::{SubmitCommand, wire::StudyCommand};
use tempo_chess::Catalog;
use tempo_store::{ReceiptResult, Snapshot, SqliteStore, Store};
use tempo_study::AttemptStatus;
use tempo_types::{DatasetId, Grade};

const JOURNEY: &str = "durable_repertoire::import_study_restart_and_replay";
const STAGE_ENV: &str = "TEMPO_TEST_REPERTOIRE_STAGE";
const DB_ENV: &str = "TEMPO_TEST_REPERTOIRE_DB";

#[test]
fn import_study_restart_and_replay() {
    if let Some(database) = env::var_os(DB_ENV) {
        run_stage(Path::new(&database));
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("repertoire.sqlite3");
    let test_binary = env::current_exe().unwrap();
    for stage in ["import-and-answer", "resume-and-replay"] {
        let output = Command::new(&test_binary)
            .args(["--exact", JOURNEY, "--test-threads=1"])
            .env(STAGE_ENV, stage)
            .env(DB_ENV, &database)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stage} failed: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn run_stage(database: &Path) {
    match env::var(STAGE_ENV).as_deref() {
        Ok("import-and-answer") => import_and_answer(database),
        Ok("resume-and-replay") => resume_and_replay(database),
        stage => panic!("unexpected child stage {stage:?}"),
    }
}

fn import_and_answer(database: &Path) {
    let dataset = DatasetId::from("durable-repertoire");
    let store = SqliteStore::create(
        database,
        dataset.clone(),
        Catalog::default(),
        daily_components(10, 0).learning.settings(),
    )
    .unwrap();
    let mut app = open(Box::new(store), 10, 0);
    let mut request = json!({
        "command": "preview", "dataset_id": dataset,
        "pgn": "1. e4 e5 2. Nf3 *", "name": "Open Games", "learner_color": "white"
    });
    let preview = app.handle_repertoire(serde_json::from_value(request.clone()).unwrap());
    assert_eq!(preview["status"], "preview");
    request["command"] = json!("save");
    request["token"] = preview["token"].clone();
    let saved = app.handle_repertoire(serde_json::from_value(request).unwrap());
    assert_eq!(saved["status"], "repertoires");
    assert_eq!(saved["repertoires"][0]["positions"], 2);
    let issued: Value = serde_json::to_value(app.handle(StudyCommand::Next)).unwrap();
    assert_eq!(issued["activity"], "teach");
    let acknowledge = json!({
        "command": "acknowledge", "dataset_id": issued["dataset_id"],
        "attempt_id": issued["attempt_id"], "expected_revision": issued["revision"],
        "submission_id": "initial-teach", "answer": { "uci": "e2e4" }
    });
    let taught = app.handle(serde_json::from_value(acknowledge).unwrap());
    assert_eq!(serde_json::to_value(taught).unwrap()["status"], "complete");
    drop(app);
    let mut app = open(Box::new(SqliteStore::open(database).unwrap()), 11, 100);
    let issued = app.next_exercise().unwrap().unwrap();
    let submission = command(&issued, "lost-answer", "d2d4");
    fs::write(
        database.with_extension("command.json"),
        serde_json::to_vec(&submission).unwrap(),
    )
    .unwrap();
    // Lose the reply after the commit, as a transport could.
    let _ = app.submit(submission).unwrap();
    let saved = app.snapshot().unwrap();
    assert_eq!(saved.learning[&issued.attempt.target].scheduled_reviews, 1);
    assert_eq!(saved.assessments.len(), 1);
    assert_eq!(saved.assessments[0].input.grade, Grade::Again);
    assert_eq!(saved.receipts.len(), 2);
    fs::write(
        database.with_extension("snapshot.json"),
        serde_json::to_vec(&saved).unwrap(),
    )
    .unwrap();
}

fn resume_and_replay(database: &Path) {
    let store = SqliteStore::open(database).unwrap();
    let saved: Snapshot =
        serde_json::from_slice(&fs::read(database.with_extension("snapshot.json")).unwrap())
            .unwrap();
    let submission: SubmitCommand =
        serde_json::from_slice(&fs::read(database.with_extension("command.json")).unwrap())
            .unwrap();
    assert_eq!(store.snapshot().unwrap(), saved);
    let mut app = open(Box::new(store), 11, 200);
    let resumed = app.next_exercise().unwrap().unwrap();
    assert_eq!(
        resumed.attempt,
        saved.attempts[&submission.attempt_id].attempt
    );
    let replayed = app.submit(submission.clone()).unwrap();
    assert_eq!(replayed.assessment, Some(Grade::Again));
    assert_eq!(
        ReceiptResult::Study(replayed),
        saved.receipts[&submission.submission_id].result
    );
    let conflicting = SubmitCommand {
        answer: "e2e4".into(),
        ..submission
    };
    assert_eq!(
        app.submit(conflicting).unwrap_err().code,
        "submission_conflict"
    );
    assert_eq!(app.snapshot().unwrap(), saved);

    let corrected = app.submit(command(&resumed, "corrected", "e2e4")).unwrap();
    assert_eq!(corrected.attempt.status, AttemptStatus::Complete);
    assert_eq!(corrected.attempt.terminal, Some(Grade::Again));
    assert_eq!(corrected.assessment, None);
    let after = app.snapshot().unwrap();
    assert_eq!(after.assessments, saved.assessments);
    assert_eq!(after.learning, saved.learning);
    let next = app.next_exercise().unwrap().unwrap();
    assert_ne!(next.attempt.id, resumed.attempt.id);
}
