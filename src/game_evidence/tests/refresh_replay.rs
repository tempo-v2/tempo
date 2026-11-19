use super::super::*;
use crate::{
    Components, FixedClock, IntroductionLimits, JsonSnapshotCodec, adapters::public_games::Game,
};
use std::path::{Path, PathBuf};
use tempo_chess::Catalog;
use tempo_learning::{DailyLearning, LearningPolicy};
use tempo_store::{SqliteStore, Store};
use tempo_types::{StudyDay, StudyTime};

const NOW: u64 = 20_000 * 86_400_000;

fn app(store: SqliteStore) -> Tempo {
    Tempo::daily(
        Box::new(store),
        Components {
            clock: Box::new(FixedClock(StudyTime {
                day: StudyDay(20_000),
                unix_ms: NOW,
            })),
            learning: Box::new(DailyLearning::default()),
            ..Components::fixtures()
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

fn fixture() -> (GameJobs, Receiver<GameRefresh>, SyncSender<RefreshResult>) {
    let (commands, provider) = sync_channel(1);
    let (response, results) = sync_channel(1);
    (
        GameJobs {
            commands,
            results,
            status: json!({"status":"idle"}),
            pending: None,
        },
        provider,
        response,
    )
}

struct Harness {
    _directory: tempfile::TempDir,
    path: PathBuf,
    app: Tempo,
    jobs: GameJobs,
    provider: Receiver<GameRefresh>,
    response: SyncSender<RefreshResult>,
}

impl Harness {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("refresh.sqlite");
        let store = SqliteStore::create(
            &path,
            DatasetId::from("dataset-a"),
            Catalog::default(),
            DailyLearning::default().settings(),
        )
        .unwrap();
        let (jobs, provider, response) = fixture();
        Self {
            _directory: directory,
            path,
            app: app(store),
            jobs,
            provider,
            response,
        }
    }

    fn begin(&mut self, id: &str) -> GameRefresh {
        assert_eq!(
            json!(
                self.jobs
                    .refresh(&self.app, intent(id, "dataset-a"), true)
                    .unwrap()
            ),
            json!({"dataset_id":"dataset-a","command_id":id,"status":"running"}),
        );
        let sent = self.provider.try_recv().unwrap();
        assert_eq!(sent.command_id, id);
        sent
    }

    fn reply(&self, sent: GameRefresh) {
        let id = sent.command_id.clone();
        self.response
            .try_send((sent, Ok(FetchedGames {
                received: 1,
                games: vec![Game {
                    provider: "lichess",
                    url: format!("https://lichess.org/{id}"),
                    id,
                    white: "learner".into(),
                    black: "other".into(),
                    start_ms: None,
                    end_ms: NOW - 1000,
                    pgn: "[White \"learner\"]\n[Black \"other\"]\n[Result \"1-0\"]\n\n1. e4 e5 1-0".into(),
                }],
                ..FetchedGames::default()
            })))
            .unwrap_or_else(|_| panic!("provider reply channel rejected fixture result"));
    }

    fn settle(&mut self, sent: GameRefresh) -> Value {
        self.reply(sent);
        let status = self.jobs.poll(&mut self.app);
        assert_eq!(status["dataset_id"], "dataset-a");
        assert_eq!(status["status"], "complete", "{status}");
        assert!(!self.jobs.active());
        status
    }

    fn accept_a(&mut self) -> Value {
        let a = self.begin("refresh-a");
        let accepted = self.settle(a);
        assert_eq!(accepted["command_id"], "refresh-a");
        assert_eq!(accepted["report"]["new_games"], 1, "{accepted}");
        accepted
    }

    fn assert_replay(&mut self, accepted_a: &Value) {
        let current = self.jobs.status.clone();
        let state = self.app.snapshot().unwrap();
        let history = self.app.game_history().unwrap();
        let sqlite_bytes = std::fs::read(&self.path).unwrap();
        let reply = self
            .jobs
            .refresh(&self.app, intent("refresh-a", "dataset-a"), false)
            .unwrap();
        assert_eq!(&json!(reply), accepted_a);
        assert_eq!(self.jobs.status, current);
        assert_eq!(self.app.snapshot().unwrap(), state);
        assert_eq!(self.app.game_history().unwrap(), history);
        assert_eq!(std::fs::read(&self.path).unwrap(), sqlite_bytes);
        self.assert_no_fetch();
    }

    fn assert_no_fetch(&self) {
        assert!(matches!(self.provider.try_recv(), Err(TryRecvError::Empty)));
    }
}

fn saved(path: &Path, id: &str) -> bool {
    let request = intent(id, "dataset-a");
    SqliteStore::open(path)
        .unwrap()
        .game_receipt(
            &request.dataset_id,
            id,
            &fingerprint("refresh", &request).unwrap(),
        )
        .unwrap()
        .is_some()
}

#[test]
fn accepted_refresh_a_replays_while_b_is_pending_without_touching_b() {
    let mut fixture = Harness::new();
    let accepted_a = fixture.accept_a();
    let b = fixture.begin("refresh-b");
    let expected_fingerprint = fingerprint("refresh", &b).unwrap();
    assert!(!saved(&fixture.path, "refresh-b"));
    // B can already own a buffered reply. Accepting A must leave it in that channel.
    fixture.reply(b);
    fixture.assert_replay(&accepted_a);
    let pending = fixture.jobs.pending.as_ref().unwrap();
    assert_eq!(pending.command_id, "refresh-b");
    assert_eq!(pending.dataset_id, DatasetId::from("dataset-a"));
    assert_eq!(pending.fingerprint, expected_fingerprint);
    let pending_b = fixture
        .jobs
        .refresh(&fixture.app, intent("refresh-b", "dataset-a"), true)
        .unwrap();
    assert_eq!(json!(pending_b), fixture.jobs.status);
    fixture.assert_no_fetch();
    let settled = fixture.jobs.poll(&mut fixture.app);
    assert_eq!(settled["command_id"], "refresh-b");
    assert_eq!(settled["status"], "complete", "{settled}");
    assert_eq!(settled["report"]["new_games"], 1, "{settled}");
    assert!(saved(&fixture.path, "refresh-b"));
    let bytes = std::fs::read(&fixture.path).unwrap();
    assert_eq!(fixture.jobs.poll(&mut fixture.app), settled);
    assert_eq!(std::fs::read(&fixture.path).unwrap(), bytes);
}

#[test]
fn accepted_refresh_a_replays_after_b_settles_without_replacing_b() {
    let mut fixture = Harness::new();
    let accepted_a = fixture.accept_a();
    let b = fixture.begin("refresh-b");
    let settled_b = fixture.settle(b);
    fixture.assert_replay(&accepted_a);
    assert_eq!(fixture.jobs.status, settled_b);
    assert!(!fixture.jobs.active());
}

#[test]
fn accepted_refresh_a_replays_after_sqlite_reopen() {
    let mut fixture = Harness::new();
    let accepted_a = fixture.accept_a();
    assert!(saved(&fixture.path, "refresh-a"));
    let Harness {
        _directory,
        path,
        app: previous,
        ..
    } = fixture;
    drop(previous);

    let (jobs, provider, response) = self::fixture();
    let mut reopened = Harness {
        app: app(SqliteStore::open(&path).unwrap()),
        _directory,
        path,
        jobs,
        provider,
        response,
    };
    let b = reopened.begin("refresh-b");
    reopened.assert_replay(&accepted_a);
    assert_eq!(reopened.settle(b)["command_id"], "refresh-b");
}

#[test]
fn refresh_replay_rejects_changed_input_and_replaced_dataset() {
    let mut fixture = Harness::new();
    fixture.accept_a();
    let _b = fixture.begin("refresh-b");
    let before = fixture.jobs.status.clone();
    let bytes = std::fs::read(&fixture.path).unwrap();
    for command in ["refresh-a", "refresh-b"] {
        let mut changed = intent(command, "dataset-a");
        changed.account = "someone-else".into();
        let error = fixture
            .jobs
            .refresh(&fixture.app, changed, true)
            .err()
            .unwrap();
        assert_eq!(error.code, "store_conflict");
    }
    let busy = fixture
        .jobs
        .refresh(&fixture.app, intent("refresh-c", "dataset-a"), true)
        .err()
        .unwrap();
    assert_eq!(busy.code, "game_refresh_busy");
    fixture.assert_no_fetch();
    assert_eq!(fixture.jobs.status, before);
    assert_eq!(std::fs::read(&fixture.path).unwrap(), bytes);

    let portable = fixture.app.export(&JsonSnapshotCodec).unwrap();
    fixture
        .app
        .restore(DatasetId::from("dataset-b"), &portable, &JsonSnapshotCodec)
        .unwrap();
    let replaced = std::fs::read(&fixture.path).unwrap();
    let old = fixture
        .jobs
        .refresh(&fixture.app, intent("refresh-a", "dataset-a"), true)
        .err()
        .unwrap();
    assert_eq!(old.code, "stale_dataset");
    assert_eq!(fixture.jobs.status, before);
    assert_eq!(std::fs::read(&fixture.path).unwrap(), replaced);
    fixture.assert_no_fetch();
}

#[test]
fn disconnected_worker_failure_keeps_its_address() {
    let mut fixture = Harness::new();
    let _sent = fixture.begin("refresh-a");
    drop(fixture.response);
    let failed = fixture.jobs.poll(&mut fixture.app);
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["dataset_id"], "dataset-a");
    assert_eq!(failed["command_id"], "refresh-a");
    assert!(!fixture.jobs.active());
}
