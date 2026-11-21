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
use tempo_chess::{Catalog, LegalSequenceGrader};
use tempo_learning::{DailyLearning, LearningPolicy};
use tempo_store::{Commit, SqliteStore, Store, Write};
use tempo_types::{StudyDay, StudyTime, TargetId};

const DATASET: &str = "connected-reviews";
const LINE: &str = "1. e4 c5 2. Nf3 d6 3. d4 cxd4 4. Nxd4 Nf6 5. Nc3 a6 6. Be3 *";
const TEST_MOVES: [&str; 3] = ["g1f3", "d2d4", "f3d4"];

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
    targets: Vec<TargetId>,
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
            targets: vec![],
        };
        let saved = f.save(pgn);
        f.repertoire = saved["repertoires"][0]["id"].as_str().unwrap().into();
        let snapshot = f.app.snapshot().unwrap();
        f.targets = snapshot.catalog.lines[0].targets.clone();
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
            ..DailyComponents::fixtures()
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
            .handle_with_repertoire_runs(serde_json::from_value::<StudyCommand>(command).unwrap());
        serde_json::to_value(reply).unwrap()
    }

    fn next(&mut self) -> Value {
        self.command(json!({"command": "next"}))
    }

    fn rank_first(&self, target: &TargetId, from: i32, exclude: &[TargetId]) {
        let snapshot = self.app.snapshot().unwrap();
        let seed = u64::from_le_bytes(
            blake3::hash(DATASET.as_bytes()).as_bytes()[..8]
                .try_into()
                .unwrap(),
        );
        let today = (from..from + 500)
            .find(|day| {
                snapshot
                    .catalog
                    .items
                    .keys()
                    .filter(|id| !exclude.contains(id))
                    .min_by_key(|id| (tempo_queue::daily_rank(seed, StudyDay(*day), id), *id))
                    == Some(target)
            })
            .expect("a deterministic clock day gives the intended ordinary first pick");
        self.clock.0.store(today, Ordering::Relaxed);
    }

    fn master(&mut self, target: &TargetId) {
        for i in 0..self.app.snapshot().unwrap().catalog.items.len() {
            let legacy = serde_json::to_value(self.app.handle(StudyCommand::Next)).unwrap();
            assert_eq!(legacy["activity"], "test", "{legacy}");
            if legacy["target_id"] == target.0 {
                let snapshot = self.app.snapshot().unwrap();
                let uci = &snapshot.catalog.items[target].definition.decisions[0]
                    .expected
                    .uci;
                let reply = self.command(address("answer", &legacy, "master", Some(uci)));
                assert_eq!(reply["assessment"], "good", "{reply}");
                self.restart(); // End the fixture's setup deferrals.
                return;
            }
            let skipped = self.command(address("skip", &legacy, &format!("setup-skip-{i}"), None));
            assert_eq!(skipped["status"], "abandoned", "{skipped}");
        }
        panic!("expected to reach the warm-up target");
    }

    fn line() -> Self {
        let mut f = Self::new(LINE);
        let root = f.targets[0].clone();
        f.rank_first(&f.targets[1], 20_001, std::slice::from_ref(&root));
        f.master(&root);
        f
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
fn a_warmup_and_three_independent_reviews_resume_on_the_same_authored_path() {
    let mut f = Fixture::line();
    let before = f.app.snapshot().unwrap();
    let warmup = f.next();
    assert_eq!(warmup["repertoire_context"], "warmup", "{warmup}");
    assert_eq!(warmup["activity"], "practice");
    assert_eq!(warmup["target_id"], f.targets[0].0);
    let legacy = serde_json::to_value(f.app.handle(StudyCommand::Next)).unwrap();
    assert_eq!(legacy["attempt_id"], warmup["attempt_id"]);
    assert!(legacy.get("repertoire_context").is_none());
    let reply = address("answer", &warmup, "warmup", Some("e2e4"));
    let mut accepted = f.command(reply.clone());
    assert_eq!(accepted["assessment"], "good");
    assert_eq!(f.app.snapshot().unwrap().learning, before.learning);
    assert_eq!(f.app.snapshot().unwrap().assessments, before.assessments);

    // A comment save changes source identity but preserves the exact path.
    f.save("1. e4 {Context} c5 2. Nf3 d6 3. d4 cxd4 4. Nxd4 Nf6 5. Nc3 a6 6. Be3 *");
    f.restart();
    assert_eq!(f.command(reply), accepted);
    for (i, (uci, opponent)) in TEST_MOVES
        .into_iter()
        .zip(["c7c5", "d7d6", "c5d4"])
        .enumerate()
    {
        let next = f.next();
        assert_eq!(next["activity"], "test", "{next}");
        assert_eq!(next["target_id"], f.targets[i + 1].0);
        assert_eq!(next["opponent_move"]["uci"], opponent);
        let bridge = tempo_chess::OpponentLeadIn::verified(
            accepted["display_fen"].as_str().unwrap(),
            opponent,
            next["display_fen"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(next["opponent_move"]["before_fen"], bridge.before_fen);
        assert!(next.get("lesson").is_none());
        assert!(next.get("repertoire_context").is_none());
        f.restart();
        assert_eq!(f.next(), next);
        let answer = address("answer", &next, &format!("test-{i}"), Some(uci));
        accepted = f.command(answer.clone());
        assert_eq!(accepted["assessment"], "good", "{accepted}");
        assert_eq!(f.command(answer), accepted);
    }
    let after = f.app.snapshot().unwrap();
    assert!(after.repertoire_run.is_none());
    assert_eq!(after.assessments.len(), before.assessments.len() + 3);
    for target in &f.targets[1..4] {
        assert_eq!(after.learning[target].scheduled_reviews, 1);
    }
    assert_eq!(
        after.learning[&f.targets[4]],
        before.learning[&f.targets[4]]
    );
    assert_eq!(
        after.learning[&f.targets[0]],
        before.learning[&f.targets[0]]
    );
}

#[test]
fn a_warmup_skip_replays_without_deferring_and_a_missed_test_returns_alone() {
    let mut f = Fixture::line();
    let warmup = f.next();
    assert_eq!(warmup["repertoire_context"], "warmup");
    let skip = address("skip", &warmup, "skip-warmup", None);
    let skipped = f.command(skip.clone());
    assert_eq!(skipped["status"], "abandoned");
    f.restart();
    assert_eq!(f.command(skip), skipped);
    let anchor = f.next();
    assert_eq!(anchor["target_id"], f.targets[1].0);
    let miss = f.command(address("answer", &anchor, "wrong-nf3", Some("b1c3")));
    assert_eq!(miss["assessment"], "again", "{miss}");
    assert!(f.app.snapshot().unwrap().repertoire_run.is_none());
    let corrected = f.command(address("answer", &miss, "correct-nf3", Some("g1f3")));
    assert_eq!(corrected["status"], "complete");
    assert_eq!(corrected["assessment"], "again");
    let after = f.app.snapshot().unwrap();
    assert_eq!(after.assessments.len(), 2);
    let next_due = after.learning[&f.targets[1]].due.unwrap().0;
    f.rank_first(&f.targets[1], next_due, &[]);
    let standalone = f.next();
    assert_eq!(standalone["target_id"], f.targets[1].0);
    assert_eq!(standalone["activity"], "test");
    assert!(f.app.snapshot().unwrap().repertoire_run.is_none());
    assert_eq!(
        f.command(address("answer", &standalone, "clean-nf3", Some("g1f3")))["assessment"],
        "good"
    );
    let due = f.app.snapshot().unwrap().learning[&f.targets[1]]
        .due
        .unwrap()
        .0;
    f.rank_first(&f.targets[1], due, &[]);
    assert_eq!(f.next()["target_id"], f.targets[1].0);
    assert!(f.app.snapshot().unwrap().repertoire_run.is_some());
}

#[test]
fn forks_and_transpositions_do_not_choose_context_from_document_order() {
    let mut fork = Fixture::new("1. e4 c5 (1... e5 2. Nf3 Nc6 3. Bb5) 2. Nf3 d6 3. d4 *");
    fork.rank_first(&fork.targets[0], 20_001, &[]);
    assert_eq!(fork.next()["target_id"], fork.targets[0].0);
    assert!(fork.app.snapshot().unwrap().repertoire_run.is_none());

    let mut transposed = Fixture::new("1. Nf3 d5 (1... Nf6 2. d4 d5 3. c4) 2. d4 Nf6 3. c4 *");
    let familiar = transposed.targets[1].clone();
    transposed.rank_first(
        &transposed.targets[2],
        20_001,
        std::slice::from_ref(&familiar),
    );
    transposed.master(&familiar);
    let view = transposed.next();
    assert_eq!(view["target_id"], transposed.targets[2].0);
    assert_eq!(view["activity"], "test");
    assert!(view.get("opponent_move").is_none());
}

#[test]
fn a_repeated_target_stops_the_run_in_either_variation_order() {
    let lines = [
        "1. Nf3 Nf6 2. Ng1 Ng8 (2... e6 3. d4) 3. Nf3 *",
        "1. Nf3 Nf6 2. Ng1 e6 (2... Ng8 3. Nf3) 3. d4 *",
    ];
    for pgn in lines {
        let mut f = Fixture::new(pgn);
        f.rank_first(&f.targets[0], 20_001, &[]);
        let first = f.next();
        assert_eq!(first["target_id"], f.targets[0].0);
        let staged = f.app.snapshot().unwrap().repertoire_run.unwrap();
        assert_eq!(
            staged
                .steps
                .iter()
                .map(|step| &step.target)
                .collect::<Vec<_>>(),
            f.targets[..2].iter().collect::<Vec<_>>()
        );
        let accepted = f.command(address("answer", &first, "nf3", Some("g1f3")));
        assert_eq!(accepted["assessment"], "good");
        let second = f.next();
        assert_eq!(second["target_id"], f.targets[1].0);
        let accepted = f.command(address("answer", &second, "ng1", Some("f3g1")));
        assert_eq!(accepted["assessment"], "good");
        let snapshot = f.app.snapshot().unwrap();
        assert!(snapshot.repertoire_run.is_none());
        assert_eq!(snapshot.assessments.len(), 2);
    }
}

#[test]
fn a_changed_answer_or_suspension_drops_unused_steps_without_touching_the_warmup_result() {
    for change in ["answer", "suspension"] {
        let mut f = Fixture::line();
        let warmup = f.next();
        let command = address("answer", &warmup, "completed-warmup", Some("e2e4"));
        let accepted = f.command(command.clone());
        if change == "answer" {
            f.save("1. e4 c5 2. Nc3 d6 3. d4 cxd4 4. Qxd4 *");
        } else {
            let reply = f.app.handle_repertoire(
                serde_json::from_value(json!({
                    "command": "set_activity", "dataset_id": DATASET, "repertoire_id": f.repertoire,
                    "command_id": "suspend-during-run", "activity": "suspended"
                }))
                .unwrap(),
            );
            assert_eq!(reply["status"], "activity_set", "{reply}");
        }
        assert_eq!(f.command(command), accepted);
        let next = f.next();
        assert_ne!(next["activity"], "test", "{next}");
        assert!(f.app.snapshot().unwrap().repertoire_run.is_none());
        assert_eq!(f.app.snapshot().unwrap().assessments.len(), 1);
    }
}

#[test]
fn a_new_game_repair_interrupts_the_pending_ordinary_review() {
    let mut f = Fixture::line();
    let warmup = f.next();
    f.command(address("answer", &warmup, "warmup", Some("e2e4")));
    let anchor = f.next();
    let accepted = f.command(address("answer", &anchor, "first-test", Some("g1f3")));
    assert_eq!(accepted["assessment"], "good");

    let request = json!({
        "command_id": "game-while-reviewing",
        "dataset_id": DATASET,
        "learner": "learner",
        "pgn": "[White \"Learner\"]\n[Black \"Other\"]\n[Date \"2024.10.04\"]\n1. e4 c5 2. Nf3 d6 3. d4 cxd4 4. Nxd4 Nf6 5. Be2 1-0"
    });
    let report = serde_json::to_value(
        f.app
            .import_game_pgn(serde_json::from_value(request).unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(report["results"][0]["effect"], "again_applied", "{report}");
    let repair = f.next();
    assert_eq!(repair["corrective_repair"], true, "{repair}");
    assert_eq!(repair["target_id"], f.targets[4].0);
    assert!(f.app.snapshot().unwrap().repertoire_run.is_none());
}

#[test]
fn visible_names_do_not_change_the_connected_command_and_learning_outcomes() {
    let mut reference = None;
    for name in ["Present", "Renamed", ""] {
        let mut f = Fixture::line();
        let mut store = SqliteStore::open(&f.db).unwrap();
        let mut seed = store.snapshot().unwrap();
        for item in seed.catalog.items.values_mut() {
            item.label = name.into();
        }
        store
            .commit(Commit {
                expected_dataset: seed.dataset,
                expected_revision: seed.revision,
                writes: vec![Write::Catalog(seed.catalog)],
            })
            .unwrap();
        let mut trace = Vec::new();
        for (i, uci) in ["e2e4", "g1f3", "d2d4", "f3d4"].into_iter().enumerate() {
            let mut view = f.next();
            assert_eq!(
                view.get("exercise_name").and_then(Value::as_str),
                (!name.is_empty()).then_some(name)
            );
            let answer = address("answer", &view, &format!("invariance-{i}"), Some(uci));
            let mut accepted = f.command(answer.clone());
            assert_eq!(f.command(answer.clone()), accepted);
            view.as_object_mut().unwrap().remove("exercise_name");
            accepted.as_object_mut().unwrap().remove("exercise_name");
            trace.push((view, answer, accepted));
        }
        let final_state = store.snapshot().unwrap();
        let outcome = (
            trace,
            final_state.assessments,
            final_state.learning,
            final_state.receipts,
        );
        if let Some(reference) = &reference {
            assert_eq!(reference, &outcome);
        } else {
            reference = Some(outcome);
        }
    }
}
