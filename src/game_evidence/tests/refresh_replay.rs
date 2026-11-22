use super::super::*;
use crate::{
    DailyComponents, FixedClock, IntroductionLimits,
    adapters::public_games::{FetchedGames, Game},
};
use tempo_chess::Catalog;
use tempo_learning::{DailyLearning, LearningPolicy};
use tempo_store::SqliteStore;
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
    let store = SqliteStore::create(
        &path,
        DatasetId::from("dataset-a"),
        Catalog::default(),
        DailyLearning::default().settings(),
    )
    .unwrap();
    let mut original = app(store);
    let request = intent("accepted", "dataset-a");
    let saved = original
        .ingest_provider_games(
            &request.dataset_id,
            &request.command_id,
            &fingerprint("refresh", &request).unwrap(),
            &request.account,
            FetchedGames {
                received: 1,
                games: vec![Game {
                    provider: "lichess",
                    url: "https://lichess.org/fixture".into(),
                    id: "fixture".into(),
                    white: "learner".into(),
                    black: "other".into(),
                    start_ms: None,
                    end_ms: NOW - 1000,
                    pgn: "[White \"learner\"]\n[Black \"other\"]\n[Result \"1-0\"]\n\n1. e4 e5 1-0"
                        .into(),
                }],
                ..FetchedGames::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(saved.new_games, 1);
    drop(original);

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
