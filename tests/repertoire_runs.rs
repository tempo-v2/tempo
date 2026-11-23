//! Real Daily Study commands against a disposable SQLite store and a fixed clock.
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
};
use tempo::{Clock, CounterIds, DailyComponents, IntroductionLimits, Tempo, wire::StudyCommand};
use tempo_chess::{Catalog, LegalSequenceGrader, OpponentLeadIn};
use tempo_learning::{DailyLearning, LearningPolicy};
use tempo_store::{
    ActivityMode, Commit, RepertoireRun, RepertoireRunStep, SqliteStore, Store, Write,
};
use tempo_study::AttemptStatus;
use tempo_types::{AttemptId, StudyDay, StudyTime, TargetId};

const DATASET: &str = "connected-reviews";
const LINE: &str = "1. e4 c5 2. Nf3 d6 3. d4 cxd4 4. Nxd4 Nf6 5. Nc3 a6 6. Be3 *";

#[derive(Clone)]
struct TestClock(Arc<AtomicI32>);

impl Clock for TestClock {
    fn now(&self) -> StudyTime {
        let day = self.0.load(Ordering::Relaxed);
        StudyTime {
            day: StudyDay(day),
            unix_ms: day as u64 * 86_400_000,
        }
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    db: PathBuf,
    clock: TestClock,
    app: Tempo,
    ids: u64,
    repertoire: String,
}

impl Fixture {
    fn new(pgn: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let db = directory.path().join("study.sqlite");
        let clock = TestClock(Arc::new(AtomicI32::new(20_000)));
        let store = SqliteStore::create(
            &db,
            DATASET.into(),
            Catalog::default(),
            DailyLearning::default().settings(),
        )
        .unwrap();
        let app = Self::open(store, clock.clone(), 0);
        let mut f = Self {
            _directory: directory,
            db,
            clock,
            app,
            ids: 0,
            repertoire: String::new(),
        };
        let saved = f.save(pgn);
        f.repertoire = saved["repertoires"][0]["id"].as_str().unwrap().into();
        let snapshot = f.app.snapshot().unwrap();
        for i in 0..snapshot.catalog.items.len() {
            let view = f.next();
            assert_eq!(view["activity"], "teach", "{view}");
            let result = f.command(address(
                "acknowledge",
                &view,
                &format!("teach-{i}"),
                view["lesson"]["move_uci"].as_str(),
            ));
            assert_eq!(result["status"], "complete", "{result}");
            assert!(result["assessment"].is_null());
        }
        f
    }

    fn open(store: SqliteStore, clock: TestClock, ids: u64) -> Tempo {
        let components = DailyComponents {
            clock: Box::new(clock),
            grader: Box::new(LegalSequenceGrader),
            ids: Box::new(CounterIds(ids)),
            learning: Box::new(DailyLearning::default()),
        };
        Tempo::daily(
            Box::new(store),
            components,
            IntroductionLimits::default(),
            [42; 32],
            10,
        )
        .unwrap()
    }

    fn restart(&mut self) {
        self.ids += 1000;
        self.app = Self::open(
            SqliteStore::open(&self.db).unwrap(),
            self.clock.clone(),
            self.ids,
        );
    }

    fn save(&mut self, pgn: &str) -> Value {
        let mut input = json!({"command": "preview", "dataset_id": DATASET,
            "name": "White", "learner_color": "white", "pgn": pgn,
            "repertoire_id": (!self.repertoire.is_empty()).then_some(&self.repertoire)});
        let preview = self
            .app
            .handle_repertoire(serde_json::from_value(input.clone()).unwrap());
        assert_eq!(preview["status"], "preview", "{preview}");
        input["command"] = json!("save");
        input["token"] = preview["token"].clone();
        let saved = self
            .app
            .handle_repertoire(serde_json::from_value(input).unwrap());
        assert_eq!(saved["status"], "repertoires", "{saved}");
        saved
    }

    fn command(&mut self, command: Value) -> Value {
        let reply = self
            .app
            .handle(serde_json::from_value::<StudyCommand>(command).unwrap());
        serde_json::to_value(reply).unwrap()
    }

    fn next(&mut self) -> Value {
        self.command(json!({"command": "next"}))
    }

    fn seed_legacy_run(&mut self, warmup: bool) -> Value {
        self.clock.0.store(20_001, Ordering::Relaxed);
        let view = self.next();
        assert_eq!(view["activity"], "test", "{view}");
        let id = AttemptId(view["attempt_id"].as_str().unwrap().into());
        let mut record = self.app.snapshot().unwrap().attempts[&id].clone();
        let mut writes = Vec::new();
        if warmup {
            self.command(address("skip", &view, "fixture-skip", None));
            record.attempt.id = "legacy-warmup".into();
            record.mode = ActivityMode::Practice;
            record.repertoire_warmup = true;
        }
        let state = self.app.snapshot().unwrap();
        if warmup {
            record.issued_revision = Some(state.revision);
            writes.push(Write::Issue(record.clone()));
            writes.push(Write::Active(Some(record.attempt.id.clone())));
        }
        let next = state
            .catalog
            .items
            .values()
            .find(|item| item.target != record.attempt.target)
            .unwrap();
        let mut store = SqliteStore::open(&self.db).unwrap();
        let source = store.repertoire_document(&self.repertoire).unwrap();
        writes.push(Write::RepertoireRun(Some(RepertoireRun {
            repertoire: self.repertoire.clone(),
            source_id: source.source_id.unwrap(),
            skipped_warmup: false,
            steps: vec![
                RepertoireRunStep {
                    target: record.attempt.target.clone(),
                    version: record.attempt.content_version,
                    lead_in: None,
                    mode: record.mode,
                    issued: Some(record.attempt.id.clone()),
                },
                RepertoireRunStep {
                    target: next.target.clone(),
                    version: next.version,
                    lead_in: Some(OpponentLeadIn {
                        before_fen: view["display_fen"].as_str().unwrap().into(),
                        uci: "g8f6".into(),
                    }),
                    mode: ActivityMode::Test,
                    issued: None,
                },
            ],
        })));
        store
            .commit(Commit {
                expected_dataset: state.dataset,
                expected_revision: state.revision,
                writes,
            })
            .unwrap();
        self.restart();
        json!({
            "dataset_id": DATASET,
            "attempt_id": record.attempt.id,
            "content_version": record.attempt.content_version,
            "revision": record.attempt.revision
        })
    }
}

fn address(command: &str, view: &Value, id: &str, uci: Option<&str>) -> Value {
    let mut payload = json!({"command": command, "dataset_id": view["dataset_id"], "attempt_id": view["attempt_id"],
        "expected_revision": view["revision"], "submission_id": id});
    if let Some(uci) = uci {
        payload["answer"] = json!({"uci": uci});
    }
    payload
}

#[test]
fn connected_run_retirement_keeps_issued_test_and_retires_continuation() {
    let mut f = Fixture::new(LINE);
    let old = f.seed_legacy_run(false);
    let before = f.app.snapshot().unwrap();
    let resumed = f.next();
    assert_eq!(resumed["attempt_id"], old["attempt_id"]);
    assert_eq!(resumed["content_version"], old["content_version"]);
    assert_eq!(resumed["revision"], old["revision"]);
    assert_eq!(resumed["activity"], "test");
    assert!(resumed.get("repertoire_context").is_none());

    let target = TargetId(resumed["target_id"].as_str().unwrap().to_owned());
    let uci = &before.catalog.items[&target].definition.decisions[0]
        .expected
        .uci;
    let command = address("answer", &resumed, "retain-test", Some(uci));
    let accepted = f.command(command.clone());
    assert_eq!(accepted["status"], "complete", "{accepted}");
    assert_eq!(accepted["assessment"], "good", "{accepted}");
    f.restart();
    assert_eq!(f.command(command), accepted);
    let next = f.next();
    assert_eq!(next["activity"], "test", "{next}");
    assert_ne!(next["attempt_id"], old["attempt_id"]);
    assert!(next.get("repertoire_context").is_none());
    let after = f.app.snapshot().unwrap();
    assert_eq!(after.assessments.len(), before.assessments.len() + 1);
    assert!(after.repertoire_run.is_none());
}

#[test]
fn connected_run_retirement_closes_legacy_warmup_without_a_grade() {
    let mut f = Fixture::new(LINE);
    let old = f.seed_legacy_run(true);
    let before = f.app.snapshot().unwrap();
    let resume = f.command(json!({"command": "resume", "attempt_id": old["attempt_id"]}));
    assert_eq!(resume["code"], "retired_activity", "{resume}");
    let answer = f.command(address("answer", &old, "retired-warmup", Some("e2e4")));
    assert_eq!(answer["code"], "retired_activity", "{answer}");
    assert_eq!(f.app.snapshot().unwrap(), before);

    let next = f.next();
    assert_eq!(next["activity"], "test", "{next}");
    assert_ne!(next["attempt_id"], old["attempt_id"]);
    let after = f.app.snapshot().unwrap();
    let id = AttemptId(old["attempt_id"].as_str().unwrap().into());
    assert_eq!(after.attempts[&id].attempt.status, AttemptStatus::Abandoned);
    assert!(after.attempts[&id].attempt.terminal.is_none());
    assert_eq!(after.assessments, before.assessments);
    assert_eq!(after.learning, before.learning);
    assert_eq!(after.receipts, before.receipts);
    assert_eq!(after.repertoire_run, before.repertoire_run);
}
