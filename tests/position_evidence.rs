//! Exercise the application-facing request/status pair on a disposable SQLite store.
use serde_json::{Value, json};
use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tempo::{explorer::ExplorerClient, position_evidence::PositionEvidence};
use tempo_chess::{LearnerColor, compile_repertoire};
use tempo_learning::{DailyLearning, LearningPolicy};
use tempo_store::{SqliteStore, Store};
const START: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

fn query(service: &PositionEvidence, input: Value) -> Value {
    let queued = service
        .start(serde_json::from_value(input.clone()).unwrap())
        .unwrap();
    assert_eq!(queued["status"], "running");
    let limit = Instant::now() + Duration::from_secs(5);
    loop {
        let reply = service
            .read(serde_json::from_value(json!({"request_id":input["request_id"]})).unwrap())
            .unwrap();
        assert_eq!(reply["fen"], input["fen"]);
        assert_eq!(reply["source"], input["source"]);
        if reply["status"] != "running" {
            return reply;
        }
        assert!(Instant::now() < limit);
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn exact_legacy_move_and_empty_explorer_are_independent_of_learning_and_model_authority() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("study.sqlite");
    let catalog = compile_repertoire(
        "1.d4 d5 2.Bf4 *",
        "book",
        "London",
        LearnerColor::White,
        "import",
    )
    .unwrap();
    let store = SqliteStore::create(
        &path,
        "dataset".into(),
        catalog,
        DailyLearning::default().settings(),
    )
    .unwrap();
    let before = store.snapshot().unwrap();
    let service = PositionEvidence::new(&path, Arc::new(ExplorerClient::new(&path)));
    let repertoire = query(
        &service,
        json!({"source":"repertoire","fen":START,"request_id":"r","repertoire_id":"book","dataset_id":"dataset"}),
    );
    assert_eq!(
        repertoire["data"]["exact"],
        json!([{"uci":"d2d4","repertoire_id":"book"}]),
        "{repertoire}"
    );
    for source in ["rated", "masters"] {
        let unavailable = query(
            &service,
            json!({"source":source,"fen":START,"request_id":source}),
        );
        assert_eq!(unavailable["code"], "token_missing");
    }
    let stale = query(
        &service,
        json!({"source":"repertoire","fen":START,"request_id":"s","dataset_id":"different"}),
    );
    assert_eq!(stale["code"], "stale_dataset");
    assert_eq!(store.snapshot().unwrap(), before);
}

#[test]
fn unpinned_game_evidence_excludes_suspended_books_without_changing_library_evidence() {
    use tempo_store::{RepertoireActivity, RepertoireActivityCommand};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("suspend-evidence.sqlite");
    let mut books =
        compile_repertoire("1. e4 *", "active", "Italian", LearnerColor::White, "pgn-a").unwrap();
    let london =
        compile_repertoire("1. d4 *", "paused", "London", LearnerColor::White, "pgn-b").unwrap();
    books.items.extend(london.items);
    books.lines.extend(london.lines);
    let dataset = "dataset".into();
    let mut store =
        SqliteStore::create(&path, dataset, books, DailyLearning::default().settings()).unwrap();
    let before = store.snapshot().unwrap();
    store
        .set_repertoire_activity(
            &RepertoireActivityCommand {
                dataset: &before.dataset,
                repertoire: "paused",
                command_id: "pause",
                activity: RepertoireActivity::Suspended,
            },
            before.revision,
            None,
        )
        .unwrap();
    let service = PositionEvidence::new(&path, Arc::new(ExplorerClient::new(&path)));
    let mut request = json!({
        "request_id":"library","source":"repertoire","fen":START,"dataset_id":"dataset","repertoire_id":"active",
        "repertoire_scope":"library",
    });
    assert_eq!(
        query(&service, request.clone())["data"]["exact"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    request["request_id"] = json!("game-gap");
    request["repertoire_scope"] = json!("game");
    let game = query(&service, request.clone());
    assert_eq!(
        game["data"]["exact"],
        json!([{"uci":"e2e4","repertoire_id":"active"}]),
        "{game}"
    );
    assert!(game["data"]["repertoire_names"]["paused"].is_null());
    request["request_id"] = json!("paused-target");
    request["repertoire_id"] = json!("paused");
    assert_eq!(query(&service, request)["code"], "stale_repertoire");
}
