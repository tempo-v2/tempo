//! Qualification with the real SQLite adapter, legal grader, and upstream FSRS.
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
};
use tempo::{Clock, Components, CounterIds, IntroductionLimits, Tempo, wire::StudyCommand};
use tempo_chess::{Catalog, LegalSequenceGrader, RepertoirePgnParser};
use tempo_learning::{DailyLearning, LearningPolicy};
use tempo_store::SqliteStore;
use tempo_types::{StudyDay, StudyTime, TargetId};

const BOOK: &str = "1. e4 {Control the centre.} e5 (1... c5 2. Nf3 d6 3. d4) 2. Nf3 Nc6 3. Bb5 a6 4. Ba4 Nf6 5. O-O Be7 6. Re1 b5 7. Bb3 d6 8. c3 O-O 9. h3 Nb8 10. d4 Nbd7 11. c4 *";
const SHORT: &str = "1. e4 e5 2. Bc4 *";

type Day = Arc<AtomicI32>;

struct TestClock(Day);

impl Clock for TestClock {
    fn now(&self) -> StudyTime {
        let day = self.0.load(Ordering::Relaxed);
        StudyTime {
            day: StudyDay(day),
            unix_ms: day as u64 * 86_400_000,
        }
    }
}

fn open(path: &Path, day: Day, limits: IntroductionLimits, generation: u8) -> Tempo {
    let learning = DailyLearning::default();
    let store = if generation > 0 {
        SqliteStore::open(path).unwrap()
    } else {
        SqliteStore::create(
            path,
            "opening-proof".into(),
            Catalog::default(),
            learning.settings(),
        )
        .unwrap()
    };
    let components = Components {
        clock: Box::new(TestClock(day)),
        grader: Box::new(LegalSequenceGrader),
        parser: Box::new(RepertoirePgnParser),
        ids: Box::new(CounterIds(u64::from(generation) * 1_000)),
        learning: Box::new(learning),
        ..Components::fixtures()
    };
    Tempo::daily(Box::new(store), components, limits, [42; 32]).unwrap()
}

fn study(app: &mut Tempo, command: Value) -> Value {
    serde_json::to_value(app.handle(serde_json::from_value::<StudyCommand>(command).unwrap()))
        .unwrap()
}

fn content(app: &mut Tempo, command: Value) -> Value {
    app.handle_repertoire(serde_json::from_value(command).unwrap())
}

fn book_request(command: &str, pgn: &str, name: &str, id: Option<&str>) -> Value {
    json!({
        "command":command, "dataset_id":"opening-proof", "name":name,
        "pgn":pgn, "learner_color":"white", "repertoire_id":id,
    })
}

fn preview_and_save(app: &mut Tempo, pgn: &str, name: &str, id: Option<&str>) -> (Value, Value) {
    let preview = content(app, book_request("preview", pgn, name, id));
    assert_eq!(preview["status"], "preview", "{preview}");
    let mut save = book_request("save", pgn, name, id);
    save["token"] = preview["token"].clone();
    let saved = content(app, save);
    assert_eq!(saved["status"], "repertoires", "{saved}");
    (preview, saved)
}

fn activity_command(command: &str, activity: &Value, submission_id: &str) -> Value {
    json!({
        "command":command, "dataset_id":activity["dataset_id"],
        "attempt_id":activity["attempt_id"], "expected_revision":activity["revision"],
        "submission_id":submission_id,
    })
}

#[test]
fn quota_teach_restart_due_fsrs_and_complete_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("openings.sqlite");
    let today = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits::default();
    let mut app = open(&db, today.clone(), limits, 0);
    let (preview, saved) = preview_and_save(&mut app, BOOK, "White", None);
    assert_eq!(preview["routes"], 2);
    assert_eq!(preview["positions"], 13);
    let id = saved["repertoires"][0]["id"].as_str().unwrap();
    let first = study(&mut app, json!({"command":"next"}));
    assert_eq!(first["activity"], "teach");
    assert_eq!(first["lesson"]["comments"][0], "Control the centre.");
    assert_eq!(first["lesson"]["move_uci"], "e2e4");
    let initial_rev = app.snapshot().unwrap().revision;
    let mut wrong = activity_command("answer", &first, "ungraded");
    wrong["answer"] = json!({"uci":"e2e3"});
    assert_eq!(study(&mut app, wrong)["code"], "wrong_activity");
    assert_eq!(app.snapshot().unwrap().revision, initial_rev);
    for index in 0..10 {
        let lesson = study(&mut app, json!({"command":"next"}));
        assert_eq!(lesson["activity"], "teach", "{lesson}");
        let mut ack = activity_command("acknowledge", &lesson, &format!("lesson-{index}"));
        ack["answer"] = json!({"uci": lesson["lesson"]["move_uci"]});
        let result = study(&mut app, ack.clone());
        assert_eq!(result["status"], "complete", "{result}");
        assert_eq!(result["assessment"], Value::Null);
        assert_eq!(study(&mut app, ack), result);
    }
    let empty = study(&mut app, json!({"command":"next"}));
    assert_eq!(empty["done"], true);
    assert_eq!(empty["repertoire_remaining"], 0);
    assert_eq!(empty["tactics_remaining"], 20);
    let learned = app.snapshot().unwrap();
    assert!(learned.assessments.is_empty());
    assert_eq!(
        learned
            .learning
            .values()
            .filter(|state| state.taught_at.is_some())
            .count(),
        10
    );
    assert!(
        learned
            .learning
            .values()
            .filter(|state| state.introduced)
            .all(|state| state.due == Some(StudyDay(20_001)) && state.scheduler_memory.is_none())
    );
    let revision = learned.revision;
    preview_and_save(&mut app, BOOK, "White", Some(id));
    assert_eq!(app.snapshot().unwrap().revision, revision);
    let renamed = content(
        &mut app,
        json!({"command":"rename","dataset_id":"opening-proof","repertoire_id":id,"name":"White openings"}),
    );
    assert_eq!(renamed["repertoires"][0]["name"], "White openings");
    assert_eq!(app.snapshot().unwrap().learning, learned.learning);
    drop(app);

    today.store(20_001, Ordering::Relaxed);
    let mut app = open(&db, today.clone(), limits, 1);
    let current = study(&mut app, json!({"command":"next"}));
    assert_eq!(current["activity"], "test");
    assert!(current.get("lesson").is_none());
    assert!(!current.to_string().contains("expected"));
    drop(app);
    let mut app = open(&db, today.clone(), limits, 2);
    assert_eq!(study(&mut app, json!({"command":"next"})), current);
    let ranked: Vec<_> = {
        let mut targets: Vec<_> = learned
            .learning
            .iter()
            .filter_map(|(id, state)| state.introduced.then_some(id.clone()))
            .collect();
        let hash = blake3::hash(b"opening-proof");
        let seed = u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap());
        targets.sort_by_cached_key(|id| {
            (
                tempo_queue::daily_rank(seed, StudyDay(20_001), id),
                id.clone(),
            )
        });
        targets
    };
    let first_target: TargetId = current["target_id"].as_str().unwrap().into();
    assert_eq!(first_target, ranked[0]);
    let mut plan = book_request("preview", SHORT, "White openings", Some(id));
    let replacement = content(&mut app, plan.clone());
    assert_eq!(replacement["changed"], 1);
    assert_eq!(replacement["removed"], 11);
    plan["command"] = json!("save");
    plan["token"] = replacement["token"].clone();
    let seen = app.snapshot().unwrap();
    for (index, target) in ranked.iter().enumerate() {
        let test = study(&mut app, json!({"command":"next"}));
        assert_eq!(test["target_id"], target.0);
        let correct = &seen.catalog.items[target].definition.decisions[0]
            .expected
            .uci;
        let mut answer = activity_command("answer", &test, &format!("test-{index}"));
        answer["answer"] = json!({"uci":correct});
        let answer_result = study(&mut app, answer.clone());
        assert_eq!(answer_result["assessment"], "good", "{answer_result}");
        assert_eq!(study(&mut app, answer), answer_result);
    }
    let reviewed = app.snapshot().unwrap();
    assert_eq!(reviewed.assessments.len(), 10);
    assert!(ranked.iter().all(|target| {
        let state = &reviewed.learning[target];
        state.scheduled_reviews == 1
            && state
                .scheduler_memory
                .as_ref()
                .is_some_and(|memory| memory.algorithm == "fsrs-6.6.2")
            && state.last_review_at.unwrap().day == StudyDay(20_001)
    }));
    // The preview was computed before the latest ten reviews and preserves them.
    let saved = content(&mut app, plan);
    assert_eq!(saved["repertoires"][0]["positions"], 2, "{saved}");
    let updated = app.snapshot().unwrap();
    let root = &updated.catalog.lines[0].targets[0];
    let changed = &updated.catalog.lines[0].targets[1];
    assert_eq!(updated.learning[root], reviewed.learning[root]);
    assert_eq!(
        updated.learning[changed].first_introduced,
        reviewed.learning[changed].first_introduced
    );
    assert!(updated.learning[changed].taught_at.is_none());
    assert!(updated.learning[changed].scheduler_memory.is_none());
    assert_eq!(updated.assessments.len(), 10);
    let lesson = study(&mut app, json!({"command":"next"}));
    assert_eq!(lesson["activity"], "teach");
    assert_eq!(lesson["lesson"]["move_uci"], "f1c4");
    let mut ack = activity_command("acknowledge", &lesson, "changed-answer");
    ack["answer"] = json!({"uci":"f1c4"});
    assert_eq!(study(&mut app, ack)["status"], "complete");
    let empty = study(&mut app, json!({"command":"next"}));
    assert_eq!(empty["done"], true);
    assert_eq!(empty["repertoire_remaining"], 10);
}

#[test]
fn skipped_frontier_is_outstanding_and_removed_activity_is_invalidated() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("skip.sqlite");
    let today = Arc::new(AtomicI32::new(20_000));
    let mut app = open(&db, today, IntroductionLimits::default(), 0);
    let (_, saved) = preview_and_save(&mut app, "1. e4 e5 2. Nf3 *", "Book", None);
    let id = saved["repertoires"][0]["id"].as_str().unwrap();
    let first = study(&mut app, json!({"command":"next"}));
    assert_eq!(
        study(&mut app, activity_command("skip", &first, "skip"))["status"],
        "abandoned"
    );
    let empty = study(&mut app, json!({"command":"next"}));
    assert_eq!(empty["done"], false);
    assert_eq!(empty["outstanding"], 1);
    let resumed = study(&mut app, json!({"command":"resume_skipped"}));
    let mut ack = activity_command("acknowledge", &resumed, "first");
    ack["answer"] = json!({"uci":"e2e4"});
    assert_eq!(study(&mut app, ack)["status"], "complete");
    let next = study(&mut app, json!({"command":"next"}));
    assert_eq!(next["lesson"]["move_uci"], "g1f3");
    preview_and_save(&mut app, "1. e4 *", "Book", Some(id));
    assert!(app.snapshot().unwrap().active_attempt.is_none());
    assert_eq!(study(&mut app, json!({"command":"next"}))["done"], true);
    let mut stale = activity_command("acknowledge", &next, "retired");
    stale["answer"] = json!({"uci":"g1f3"});
    assert_eq!(study(&mut app, stale)["code"], "stale_content");
}

#[test]
fn content_save_receipt_replays_after_restart_and_rename() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("content-replay.sqlite");
    let today = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits::default();
    let mut app = open(&db, today.clone(), limits, 0);
    let pgn = "1. e4 *";
    let preview = content(&mut app, book_request("preview", pgn, "Before", None));
    let mut save = book_request("save", pgn, "Before", None);
    save["token"] = preview["token"].clone();
    let reply = content(&mut app, save.clone());
    let id = reply["repertoires"][0]["id"].as_str().unwrap();
    let renamed = content(
        &mut app,
        json!({
            "command":"rename","dataset_id":"opening-proof","repertoire_id":id,"name":"After"
        }),
    );
    assert_eq!(renamed["repertoires"][0]["name"], "After");
    drop(app);
    let mut app = open(&db, today, limits, 1);
    assert_eq!(content(&mut app, save.clone()), reply);
    save["pgn"] = json!("1. d4 *");
    assert_eq!(content(&mut app, save)["code"], "stale_preview");
    assert_eq!(content(&mut app, json!({"command":"list"})), renamed);
}
