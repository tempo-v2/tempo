use super::super::*;
use crate::{DailyComponents, FixedClock, IntroductionLimits};
use tempo_chess::Catalog;
use tempo_learning::{DailyLearning, LearningPolicy};
use tempo_store::{Commit, GameReceipt, SqliteStore, Store};
use tempo_types::{StudyDay, StudyTime};

const NOW: u64 = 20_000 * 86_400_000;

fn app(store: SqliteStore) -> Tempo {
    Tempo::daily(
        Box::new(store),
        DailyComponents {
            clock: Box::new(FixedClock(StudyTime {
                day: StudyDay(20_000),
                unix_ms: NOW,
            })),
            learning: Box::new(DailyLearning::default()),
            ..crate::components::test_components()
        },
        IntroductionLimits::default(),
        [42; 32],
        0,
    )
    .unwrap()
}

fn intent(id: &str, dataset: &str) -> GameRefresh {
    GameRefresh {
        command_id: id.into(),
        dataset_id: DatasetId::from(dataset),
        source: Provider::Lichess,
        account: "learner".into(),
        from_ms: NOW - 86_400_000,
        to_ms: NOW,
        repertoire_id: None,
    }
}

#[test]
fn exact_accepted_refresh_replays_after_restart_without_writing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("refresh.sqlite");
    let mut store = SqliteStore::create(
        &path,
        DatasetId::from("dataset-a"),
        Catalog::default(),
        DailyLearning::default().settings(),
    )
    .unwrap();
    let request = intent("accepted", "dataset-a");
    let saved = GameReport {
        fetched: 1,
        errors: vec!["Archived partial provider result.".into()],
        ..GameReport::default()
    };
    let before = store.snapshot().unwrap();
    store
        .commit_games(
            Commit {
                expected_dataset: before.dataset,
                expected_revision: before.revision,
                writes: vec![],
            },
            &[],
            None,
            GameReceipt {
                id: &request.command_id,
                fingerprint: &fingerprint("refresh", &request).unwrap(),
                report: &serde_json::to_string(&saved).unwrap(),
            },
        )
        .unwrap();
    drop(store);

    let reopened = app(SqliteStore::open(&path).unwrap());
    let before = std::fs::read(&path).unwrap();
    let (status, complete) = request.replay(&reopened).unwrap();
    assert_eq!(status, 202);
    assert_eq!(
        complete,
        json!({"status":"complete","dataset_id":"dataset-a","command_id":"accepted","report":saved})
    );
    let (status, retired) = intent("undelivered", "dataset-a")
        .replay(&reopened)
        .unwrap();
    assert_eq!(status, 410);
    assert_eq!(retired["code"], "retired_feature");
    assert_eq!(retired["command_id"], "undelivered");
    assert_eq!(retired["dataset_id"], "dataset-a");

    let mut changed = intent("accepted", "dataset-a");
    changed.account = "other".into();
    assert_eq!(
        changed.replay(&reopened).unwrap_err().code,
        "store_conflict"
    );
    assert_eq!(
        intent("accepted", "other-dataset")
            .replay(&reopened)
            .unwrap_err()
            .code,
        "stale_dataset"
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn exact_accepted_reprocess_replays_after_restart_without_writing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("reprocess.sqlite");
    let mut store = SqliteStore::create(
        &path,
        DatasetId::from("dataset-a"),
        Catalog::default(),
        DailyLearning::default().settings(),
    )
    .unwrap();
    let intent = |id: &str, dataset: &str| ReprocessGames {
        command_id: id.into(),
        dataset_id: DatasetId::from(dataset),
        learner: "learner".into(),
        from_ms: NOW - 86_400_000,
        to_ms: NOW,
    };
    let request = intent("accepted", "dataset-a");
    let saved = GameReport {
        fetched: 1,
        ..GameReport::default()
    };
    let before = store.snapshot().unwrap();
    store
        .commit_games(
            Commit {
                expected_dataset: before.dataset,
                expected_revision: before.revision,
                writes: vec![],
            },
            &[],
            None,
            GameReceipt {
                id: &request.command_id,
                fingerprint: &fingerprint("reprocess", &request).unwrap(),
                report: &serde_json::to_string(&saved).unwrap(),
            },
        )
        .unwrap();
    drop(store);

    let reopened = app(SqliteStore::open(&path).unwrap());
    let before = reopened.snapshot().unwrap();
    assert_eq!(request.replay(&reopened).unwrap(), (200, json!(saved)));
    let (status, retired) = intent("undelivered", "dataset-a").replay(&reopened).unwrap();
    assert_eq!(status, 410);
    assert_eq!(retired["code"], "retired_feature");
    assert_eq!(retired["command_id"], "undelivered");
    assert_eq!(retired["dataset_id"], "dataset-a");

    let mut changed = intent("accepted", "dataset-a");
    changed.learner = "other".into();
    assert_eq!(changed.replay(&reopened).unwrap_err().code, "store_conflict");
    assert_eq!(
        intent("accepted", "other-dataset").replay(&reopened).unwrap_err().code,
        "stale_dataset"
    );
    assert_eq!(reopened.snapshot().unwrap(), before);
}

#[test]
fn exact_accepted_choice_replays_after_reopen_and_new_choice_cannot_write() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("choice.sqlite");
    std::fs::write(&path, include_bytes!("../../../tests/fixtures/schema9-choice.sqlite")).unwrap();
    let fixture: Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/schema9-choice.json")).unwrap();
    drop(SqliteStore::open(&path).unwrap());
    let reopened = app(SqliteStore::open(&path).unwrap());
    let before = reopened.snapshot().unwrap();
    let before_db = std::fs::read(&path).unwrap();
    let request = || serde_json::from_value::<ChooseGameCorrection>(fixture["command"].clone()).unwrap();

    let (status, reply) = request().replay(&reopened).unwrap();
    assert_eq!(status, 200);
    assert_eq!(reply, fixture["accepted_reply"]);

    let mut new = request();
    new.command_id = "unaccepted-choice".into();
    let (status, reply) = new.replay(&reopened).unwrap();
    assert_eq!(status, 410);
    assert_eq!(reply["code"], "retired_feature");
    assert_eq!(reply["retryable"], false);
    assert_eq!(reply["command_id"], "unaccepted-choice");
    assert_eq!(reply["dataset_id"], "opening-proof");

    let mut changed = request();
    changed.ply += 1;
    assert_eq!(changed.replay(&reopened).unwrap_err().code, "store_conflict");
    let mut stale = request();
    stale.dataset_id = "other-dataset".into();
    assert_eq!(stale.replay(&reopened).unwrap_err().code, "stale_dataset");

    assert_eq!(reopened.snapshot().unwrap(), before);
    assert_eq!(std::fs::read(&path).unwrap(), before_db);
}
