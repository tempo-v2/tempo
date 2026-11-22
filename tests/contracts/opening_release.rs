//! Qualification with the real SQLite adapter, legal grader, and upstream FSRS.
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
};
use tempo::{Clock, CounterIds, DailyComponents, IntroductionLimits, Tempo, wire::StudyCommand};
use tempo_chess::{Catalog, ImportRequest, LearnerColor, LegalSequenceGrader, compile_repertoire};
use tempo_learning::{DailyLearning, LearningPolicy};
use tempo_store::{SqliteStore, Store};
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
    let components = DailyComponents {
        clock: Box::new(TestClock(day)),
        grader: Box::new(LegalSequenceGrader),
        ids: Box::new(CounterIds(u64::from(generation) * 1_000)),
        learning: Box::new(learning),
        ..DailyComponents::fixtures()
    };
    Tempo::daily(Box::new(store), components, limits, [42; 32], 10).unwrap()
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
    assert_eq!(first["schema_version"], 5);
    assert_eq!(first["exercise_name"], "White");
    assert!(first.get("repertoire_name").is_none());
    assert_eq!(saved["schema_version"], 4);
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
    assert_eq!(current["exercise_name"], "White openings");
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
        assert_eq!(answer_result["exercise_name"], "White openings");
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
fn daily_generic_import_rejects_repertoires_and_mixed_catalogs_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("import-boundary.sqlite");
    let today = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits::default();
    let mut app = open(&db, today.clone(), limits, 0);
    let (_, saved) = preview_and_save(&mut app, "1. e4 *", "Book", None);
    let id = saved["repertoires"][0]["id"].as_str().unwrap();
    let lesson = study(&mut app, json!({"command":"next"}));
    let mut ack = activity_command("acknowledge", &lesson, "initial-teaching");
    ack["answer"] = json!({"uci":"e2e4"});
    assert_eq!(study(&mut app, ack)["status"], "complete");

    let before = app.snapshot().unwrap();
    let request = ImportRequest {
        pgn: "1. d4 *".into(),
        source: "fixture".into(),
        repertoire: Some(id.into()),
        learner_color: Some(LearnerColor::White),
    };
    assert_eq!(
        app.import_pgn(&before.dataset, &request).unwrap_err().code,
        "unsupported"
    );

    let replacement = compile_repertoire(
        &request.pgn,
        id,
        "Book",
        LearnerColor::White,
        &request.source,
    )
    .unwrap();
    let tactics = tempo_chess::compile_tactics(&[tempo_chess::TacticRecord {
        namespace: "fixture".into(),
        source_id: "daily-tactic".into(),
        solver_fen: "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1".into(),
        solution: vec!["d2d4".into()],
        label: "Book".into(),
        provenance: vec![],
    }])
    .unwrap();
    let mut mixed = replacement.clone();
    mixed.items.extend(tactics.items.clone());
    let mut mixed_without_routes = mixed.clone();
    mixed_without_routes.lines.clear();
    for incoming in [replacement, mixed, mixed_without_routes] {
        incoming.validate().unwrap();
        assert_eq!(
            app.apply_catalog(&before.dataset, &incoming)
                .unwrap_err()
                .code,
            "content_change_error"
        );
        assert_eq!(app.snapshot().unwrap(), before);
    }
    drop(app);
    let mut app = open(&db, today, limits, 1);
    assert_eq!(app.snapshot().unwrap(), before);

    app.apply_catalog(&before.dataset, &tactics).unwrap();
    let after = app.snapshot().unwrap();
    assert_eq!(after.catalog.lines, before.catalog.lines);
    for (id, item) in &before.catalog.items {
        assert_eq!(&after.catalog.items[id], item);
        assert_eq!(after.learning[id], before.learning[id]);
    }
    assert_eq!(after.catalog.items.len(), before.catalog.items.len() + 1);
}

#[test]
fn skipped_frontier_is_outstanding_and_removed_activity_is_invalidated() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("skip.sqlite");
    let today = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits::default();
    let mut app = open(&db, today.clone(), limits, 0);
    let pgn = "1. e4 e5 2. Nf3 *";
    let (_, saved) = preview_and_save(&mut app, pgn, "Book", None);
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
    let accepted = study(&mut app, ack.clone());
    assert_eq!(accepted["status"], "complete");
    let next = study(&mut app, json!({"command":"next"}));
    assert_eq!(next["lesson"]["move_uci"], "g1f3");
    let target: TargetId = next["target_id"].as_str().unwrap().into();
    let before = app.snapshot().unwrap();
    preview_and_save(&mut app, "1. e4 *", "Book", Some(id));
    assert!(app.snapshot().unwrap().active_attempt.is_none());
    assert_eq!(study(&mut app, json!({"command":"next"}))["done"], true);
    let mut stale = activity_command("acknowledge", &next, "retired");
    stale["answer"] = json!({"uci":"g1f3"});
    assert_eq!(study(&mut app, stale.clone())["code"], "stale_content");
    drop(app);

    let mut app = open(&db, today, limits, 1);
    preview_and_save(&mut app, pgn, "Book", Some(id));
    let restored = app.snapshot().unwrap();
    assert_eq!(
        restored.catalog.items[&target],
        before.catalog.items[&target]
    );
    assert_eq!(restored.learning[&target], before.learning[&target]);
    let new_lesson = study(&mut app, json!({"command":"next"}));
    assert_eq!(new_lesson["target_id"], next["target_id"]);
    assert_ne!(new_lesson["attempt_id"], next["attempt_id"]);
    let before_retries = app.snapshot().unwrap();
    assert_eq!(study(&mut app, stale)["status"], "error");
    assert_eq!(study(&mut app, ack), accepted);
    assert_eq!(app.snapshot().unwrap(), before_retries);
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

fn tactic_capture(app: &mut Tempo, command: Value) -> Value {
    app.handle_tactic_capture(serde_json::from_value(command).unwrap())
}

fn save_tactics(
    app: &mut Tempo,
    pgn: &str,
    selections: Value,
    selected: Vec<usize>,
) -> (Value, Value) {
    let mut request = json!({
        "command":"preview", "dataset_id":"opening-proof", "pgn":pgn, "selections":selections
    });
    let preview = tactic_capture(app, request.clone());
    assert_eq!(preview["status"], "tactics_preview", "{preview}");
    request["command"] = json!("save");
    request["token"] = preview["token"].clone();
    request["selected"] = json!(selected);
    let reply = tactic_capture(app, request.clone());
    assert_eq!(reply["status"], "tactics_saved", "{reply}");
    assert_eq!(tactic_capture(app, request), reply);
    (preview, reply)
}

fn transfer_tactics(app: &mut Tempo, pgn: &str, selected: &[usize], provenance: &str) {
    let catalog = tempo_chess::preview_tactic_pgn(pgn, &[], provenance)
        .unwrap()
        .selected_catalog(selected)
        .unwrap();
    app.apply_tactic_transfer(&catalog).unwrap();
}

fn snippet(label: &str, fen: &str, moves: &str) -> String {
    format!("[Event \"{label}\"]\n[SetUp \"1\"]\n[FEN \"{fen}\"]\n\n{moves} *\n\n")
}

#[test]
fn mixed_study_independent_quotas_due_order_and_optional_actions() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("mixed.sqlite");
    let today = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits::default();
    let mut app = open(&db, today.clone(), limits, 0);
    preview_and_save(&mut app, BOOK, "Openings", None);
    let initial = [
        "a3", "a4", "b3", "b4", "c3", "c4", "d3", "d4", "e3", "e4", "f3", "f4", "g3", "g4", "h3",
        "h4", "Na3", "Nc3", "Nf3", "Nh3",
    ];
    let mut pgn = initial
        .into_iter()
        .map(|san| snippet(san, tempo_chess::START_FEN, &format!("1. {san}")))
        .collect::<String>();
    let black = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1";
    for san in ["a6", "a5", "b6", "b5", "c6"] {
        pgn.push_str(&snippet(san, black, &format!("1... {san}")));
    }
    transfer_tactics(&mut app, &pgn, &(0..25).collect::<Vec<_>>(), "test catalog");
    let imported = app.snapshot().unwrap();
    let grade = |app: &mut Tempo, card: &Value, id: &str| {
        let target: TargetId = card["target_id"].as_str().unwrap().into();
        let mut cmd = activity_command("answer", card, id);
        cmd["answer"] =
            json!({"uci": imported.catalog.items[&target].definition.decisions[0].expected.uci});
        study(app, cmd)
    };
    let mut openings = 0;
    let mut tactics = 0;
    for index in 0..30 {
        let card = study(&mut app, json!({"command":"next"}));
        let expected_teach = index < 20 && index % 2 == 0;
        assert_eq!(
            card["activity"],
            if expected_teach { "teach" } else { "test" },
            "{card}"
        );
        if expected_teach {
            let mut ack = activity_command("acknowledge", &card, &format!("lesson-{index}"));
            ack["answer"] = json!({"uci":card["lesson"]["move_uci"]});
            assert_eq!(study(&mut app, ack)["status"], "complete");
            openings += 1;
        } else {
            assert!(card.get("lesson").is_none());
            assert!(card.get("exercise_name").is_none());
            if index == 1 {
                let give_up = activity_command("give_up", &card, "learn-first-miss");
                let miss = study(&mut app, give_up.clone());
                assert_eq!(miss["assessment"], "again");
                assert_eq!(study(&mut app, give_up), miss);
                // Continue correction in the existing activity.
                let corrected = grade(&mut app, &miss, "learn-first-correction");
                assert_eq!(corrected["status"], "complete");
                assert_eq!(corrected["assessment"], "again");
            } else {
                assert_eq!(
                    grade(&mut app, &card, &format!("introduced-{index}"))["assessment"],
                    "good"
                );
            }
            tactics += 1;
        }
    }
    assert_eq!((openings, tactics), (10, 20));
    let done = study(&mut app, json!({"command":"next"}));
    assert_eq!(done["done"], true, "{done}");
    assert_eq!(done["repertoire_remaining"], 0);
    assert_eq!(done["tactics_remaining"], 0);
    assert_eq!(done["can_practice_tactics"], true);
    assert_eq!(done["can_learn_more_tactics"], true);
    let admitted = app.snapshot().unwrap();
    assert_eq!(admitted.assessments.len(), 20);
    let practice = study(&mut app, json!({"command":"practice_tactics"}));
    assert_eq!(practice["activity"], "practice");
    let target: TargetId = practice["target_id"].as_str().unwrap().into();
    assert!(admitted.learning[&target].introduced);
    assert_eq!(grade(&mut app, &practice, "practice")["assessment"], "good");
    let after_practice = app.snapshot().unwrap();
    assert_eq!(after_practice.learning, admitted.learning);
    assert_eq!(after_practice.assessments, admitted.assessments);
    let learn_more =
        json!({"command":"learn_more_tactics","dataset_id":"opening-proof","submission_id":"more"});
    let extended = study(&mut app, learn_more.clone());
    assert_eq!(extended["done"], false);
    assert_eq!(extended["tactics_remaining"], 10);
    assert_eq!(extended["outstanding"], 5);
    drop(app);

    let mut app = open(&db, today.clone(), limits, 1);
    assert_eq!(study(&mut app, learn_more), extended);
    for index in 0..5 {
        let card = study(&mut app, json!({"command":"next"}));
        assert_eq!(card["activity"], "test");
        let target: TargetId = card["target_id"].as_str().unwrap().into();
        assert!(!admitted.learning[&target].introduced);
        assert_eq!(
            grade(&mut app, &card, &format!("optional-{index}"))["assessment"],
            "good"
        );
    }
    let done = study(&mut app, json!({"command":"next"}));
    assert_eq!(done["done"], true);
    assert_eq!(done["tactics_remaining"], 5);
    assert_eq!(done["can_learn_more_tactics"], false);
    let all_introduced = app.snapshot().unwrap();

    today.store(20_014, Ordering::Relaxed);
    let mut due = all_introduced
        .learning
        .iter()
        .filter_map(|(id, state)| state.introduced.then_some(id.clone()))
        .collect::<Vec<_>>();
    assert_eq!(due.len(), 35);
    let hash = blake3::hash(b"opening-proof");
    let seed = u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap());
    due.sort_by_cached_key(|id| {
        (
            tempo_queue::daily_rank(seed, StudyDay(20_014), id),
            id.clone(),
        )
    });
    for (index, target) in due.iter().enumerate() {
        let review = study(&mut app, json!({"command":"next"}));
        assert_eq!(review["activity"], "test");
        assert_eq!(review["target_id"], target.0);
        if index == 6 {
            drop(app);
            app = open(&db, today.clone(), limits, 2);
            assert_eq!(study(&mut app, json!({"command":"next"})), review);
        }
        assert_eq!(
            grade(&mut app, &review, &format!("due-{index}"))["assessment"],
            "good"
        );
    }
    let status = study(&mut app, json!({"command":"study_status"}));
    assert_eq!(status["tactics_remaining"], 20);
    assert_eq!(status["repertoire_remaining"], 10);
    assert_eq!(app.snapshot().unwrap().assessments.len(), 60);
    for index in 0..40 {
        preview_and_save(&mut app, BOOK, &format!("Timing sample {index}"), None);
    }
    assert_eq!(app.snapshot().unwrap().catalog.items.len(), 558);
    let start = std::time::Instant::now();
    let current = study(&mut app, json!({"command":"next"}));
    let issued_in = start.elapsed();
    assert_eq!(current["activity"], "teach");
    let current_id = current["attempt_id"].as_str().unwrap().into();
    let scoped = tempo_store::Store::study_state(
        &SqliteStore::open(&db).unwrap(),
        tempo_store::StudyScope {
            attempt: Some(&current_id),
            ..tempo_store::StudyScope::default()
        },
    )
    .unwrap();
    let target = &scoped.attempts[&current_id].attempt.target;
    assert_eq!(
        scoped.target_coverage(),
        &tempo_store::TargetCoverage::Selected([target.clone()].into())
    );
    assert!(scoped.target(target).unwrap().is_some());
    assert!(scoped.complete_catalog().is_err());
    assert_eq!(scoped.attempts.len(), 1);
    assert!(scoped.assessments.is_empty() && scoped.receipts.is_empty());
    eprintln!(
        "Diagnostic: 558 targets and 60 scheduled outcomes; next issuance {issued_in:?}; current-command read returned one activity, target, and learning state."
    );
}

#[test]
fn capture_selects_a_game_variation_and_rejects_invalid_selected_rows_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("capture.sqlite");
    let today = Arc::new(AtomicI32::new(20_000));
    let mut app = open(&db, today, IntroductionLimits::default(), 0);
    let pgn = "[Event \"Game\"]\n\n1. e4 e5 (1... c5 2. Nf3 d6) 2. Nc3 Nc6 *\n\n[Event \"Invalid\"]\n\n1. Kc2 *";
    let preview = tactic_capture(
        &mut app,
        json!({"command":"preview","dataset_id":"opening-proof","pgn":pgn}),
    );
    assert_eq!(preview["rows"][0]["status"], "selection_required");
    assert_eq!(preview["rows"][1]["status"], "invalid");
    let route = preview["rows"][0]["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|route| route["moves"][1]["san"] == "c5")
        .unwrap()["route"]
        .clone();
    let selections = json!([{"record":0,"route":route,"start":2,"end":4}]);
    let preview = tactic_capture(
        &mut app,
        json!({"command":"preview","dataset_id":"opening-proof","pgn":pgn,"selections":selections}),
    );
    assert_eq!(preview["rows"][0]["solution"], json!(["g1f3", "d7d6"]));
    let before = app.snapshot().unwrap();
    let rejected = tactic_capture(
        &mut app,
        json!({"command":"save","dataset_id":"opening-proof","pgn":pgn,
        "selections":selections,"token":preview["token"],"selected":[0,1]}),
    );
    assert_eq!(rejected["code"], "invalid_selection");
    assert_eq!(app.snapshot().unwrap(), before);
    save_tactics(&mut app, pgn, selections, vec![0]);
    let issued = study(&mut app, json!({"command":"next"}));
    assert_eq!(issued["display_fen"], preview["rows"][0]["solver_fen"]);
    let mut answer = activity_command("answer", &issued, "game segment");
    answer["answer"] = json!({"uci":"g1f3"});
    let solved = study(&mut app, answer);
    assert_eq!(solved["assessment"], "good");
    assert_eq!(solved["feedback"]["autoplay"], json!([{"uci":"d7d6"}]));
}

#[test]
fn an_issued_lesson_crosses_midnight_even_after_lowering_the_new_quota() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("day-change.sqlite");
    let today = Arc::new(AtomicI32::new(20_000));
    let mut app = open(&db, today.clone(), IntroductionLimits::default(), 0);
    preview_and_save(&mut app, "1. e4 e5 2. Nf3 *", "One route", None);
    let issued = study(&mut app, json!({"command":"next"}));
    assert_eq!(issued["activity"], "teach");
    assert_eq!(
        study(&mut app, json!({"command":"study_status"}))["outstanding"],
        1
    );
    drop(app);
    today.store(20_001, Ordering::Relaxed);
    let mut app = open(
        &db,
        today,
        IntroductionLimits {
            repertoire: 0,
            tactics: 20,
        },
        1,
    );
    assert_eq!(study(&mut app, json!({"command":"next"})), issued);
    assert_eq!(
        study(&mut app, json!({"command":"study_status"}))["outstanding"],
        1
    );
    let mut ack = activity_command("acknowledge", &issued, "cross-midnight");
    ack["answer"] = json!({"uci":"e2e4"});
    assert_eq!(study(&mut app, ack)["status"], "complete");
    let target = issued["target_id"].as_str().unwrap().into();
    let snapshot = app.snapshot().unwrap();
    assert_eq!(
        snapshot.learning[&target].first_introduced,
        Some(StudyDay(20_001))
    );
    assert_eq!(snapshot.learning[&target].due, Some(StudyDay(20_002)));
    assert_eq!(study(&mut app, json!({"command":"next"}))["done"], true);
}

#[test]
fn a_wire_four_allowance_receipt_replays_after_rollover_without_a_write() {
    use tempo_store::{Receipt, ReceiptResult, Store};

    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("wire-four.sqlite");
    let day = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits {
        repertoire: 0,
        tactics: 1,
    };
    let app = open(&db, day.clone(), limits, 0);
    let snapshot = app.snapshot().unwrap();
    drop(app);

    let saved = json!({
        "schema_version": 4, "status": "empty", "dataset_id": snapshot.dataset,
        "done": false, "outstanding": 3, "repertoire_remaining": 0, "tactics_remaining": 10,
        "can_practice_tactics": false, "can_learn_more_tactics": false, "learn_more_count": 0,
    });
    let extend = json!({
        "command": "learn_more_tactics",
        "dataset_id": snapshot.dataset,
        "submission_id": "accepted-on-wire-four",
    });
    SqliteStore::open(&db)
        .unwrap()
        .extend_tactics_allowance(
            &snapshot.dataset,
            snapshot.revision,
            StudyDay(20_000),
            10,
            &Receipt {
                id: "accepted-on-wire-four".into(),
                command: json!(["learn_more_tactics", snapshot.dataset]).to_string(),
                result: ReceiptResult::TacticsAllowance {
                    reply: saved.to_string(),
                },
            },
        )
        .unwrap();

    day.store(20_001, Ordering::Relaxed);
    let mut app = open(&db, day, limits, 1);
    let before = app.snapshot().unwrap();
    let mut expected = saved;
    expected["schema_version"] = json!(5);
    assert_eq!(study(&mut app, extend.clone()), expected);
    assert_eq!(study(&mut app, extend), expected);
    assert_eq!(app.snapshot().unwrap(), before);
    let store = SqliteStore::open(&db).unwrap();
    assert_eq!(
        store
            .tactics_extra(&snapshot.dataset, StudyDay(20_000))
            .unwrap(),
        10
    );
    assert_eq!(
        store
            .tactics_extra(&snapshot.dataset, StudyDay(20_001))
            .unwrap(),
        0
    );
}

#[test]
fn practice_failure_and_cross_midnight_allowance_replay_preserve_learning() {
    use tempo_store::Store;

    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("optional-tactics.sqlite");
    let day = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits {
        repertoire: 0,
        tactics: 1,
    };
    let mut app = open(&db, day.clone(), limits, 0);
    let position = "4k3/8/8/8/8/8/4P3/4K3 w - -";
    let original = snippet("Original tactic", &format!("{position} 0 1"), "e4");
    transfer_tactics(&mut app, &original, &[0], "test catalog");
    let scheduled = study(&mut app, json!({"command":"next"}));
    let mut solve = activity_command("answer", &scheduled, "introduce-first");
    solve["answer"] = json!({"uci":"e2e4"});
    assert_eq!(study(&mut app, solve)["assessment"], "good");

    let learned = app.snapshot().unwrap();
    let same_line = snippet("Same tactic", &format!("{position} 12 20"), "e4");
    save_tactics(&mut app, &same_line, json!([]), vec![0]);
    let reimported = app.snapshot().unwrap();
    assert_eq!(reimported.catalog.items.len(), 1);
    assert_eq!(reimported.learning, learned.learning);
    let other = snippet("Other tactic", &format!("{position} 0 1"), "e3");
    transfer_tactics(&mut app, &other, &[0], "test catalog");

    let done = study(&mut app, json!({"command":"study_status"}));
    assert_eq!(done["done"], true);
    assert_eq!(done["tactics_remaining"], 0);
    let before_practice = app.snapshot().unwrap();
    let practice = study(&mut app, json!({"command":"practice_tactics"}));
    assert_eq!(practice["target_id"], scheduled["target_id"]);
    let give_up = activity_command("give_up", &practice, "practice-reveal");
    let failed = study(&mut app, give_up.clone());
    assert_eq!(failed["assessment"], "again");
    assert_eq!(study(&mut app, give_up), failed);
    let mut correct = activity_command("answer", &failed, "practice-correction");
    correct["answer"] = json!({"uci":"e2e4"});
    let completed = study(&mut app, correct);
    assert_eq!(completed["status"], "complete");
    assert_eq!(completed["assessment"], "again");
    let after_practice = app.snapshot().unwrap();
    assert_eq!(after_practice.learning, before_practice.learning);
    assert_eq!(after_practice.assessments, before_practice.assessments);
    assert_eq!(study(&mut app, json!({"command":"study_status"})), done);

    let extend = json!({
        "command":"learn_more_tactics",
        "dataset_id":"opening-proof",
        "submission_id":"accepted-before-midnight"
    });
    let extended = study(&mut app, extend.clone());
    assert_eq!(extended["tactics_remaining"], 10);
    assert_eq!(extended["outstanding"], 1);
    let progress = app.study_progress(None).unwrap().unwrap();
    assert_eq!(progress.tactics_introduced, 1);
    assert_eq!(progress.tactics_limit, 11);
    assert_eq!(progress.extra_tactics, 10);
    assert!(app.study_progress(Some(&progress.stamp)).unwrap().is_none());
    drop(app);
    day.store(20_001, Ordering::Relaxed);
    let mut app = open(&db, day, limits, 1);
    let before_replay = app.snapshot().unwrap();
    let today = app.study_progress(Some(&progress.stamp)).unwrap().unwrap();
    assert_eq!(today.stamp.day, StudyDay(20_001));
    assert_eq!(today.tactics_introduced, 0);
    assert_eq!(today.tactics_limit, 1);
    assert_eq!(today.extra_tactics, 0);
    assert_eq!(study(&mut app, extend), extended);
    assert_eq!(app.snapshot().unwrap(), before_replay);
    let store = SqliteStore::open(&db).unwrap();
    assert_eq!(
        store
            .tactics_extra(&before_replay.dataset, StudyDay(20_000))
            .unwrap(),
        10
    );
    assert_eq!(
        store
            .tactics_extra(&before_replay.dataset, StudyDay(20_001))
            .unwrap(),
        0
    );
    assert_eq!(
        study(&mut app, json!({"command":"study_status"}))["tactics_remaining"],
        1
    );
}

#[test]
fn later_tactic_correction_uses_the_issued_move_regardless_of_capture_attribution() {
    use tempo_chess::{GameCapture, TacticRecord, compile_tactics};

    for from_game in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let db = directory.path().join("later-miss.sqlite");
        let mut app = open(
            &db,
            Arc::new(AtomicI32::new(20_000)),
            IntroductionLimits {
                repertoire: 0,
                tactics: 1,
            },
            0,
        );
        let primary = TacticRecord {
            namespace: "explanation-fixture".into(),
            source_id: "later-pin".into(),
            solver_fen: "4k3/7p/2n5/8/8/8/4B3/6K1 w - - 0 1".into(),
            solution: ["g1h1", "h7h6", "e2b5"].map(str::to_owned).to_vec(),
            label: "A tactic from a game".into(),
            provenance: vec![
                "tempo1:capture:manual".into(),
                "tempo1:source_kind:manual".into(),
            ],
        };
        let alias = TacticRecord {
            source_id: "same-pin-with-an-unattached-tag".into(),
            provenance: vec!["tempo1:source_kind:game".into()],
            ..primary.clone()
        };
        let target = primary.target_id();
        let mut catalog = compile_tactics(&[primary, alias]).unwrap();
        if from_game {
            catalog.items.get_mut(&target).unwrap().game_capture = Some(GameCapture {
                capture_id: "reviewed-capture-record".into(),
            });
        }
        app.apply_tactic_transfer(&catalog).unwrap();
        let imported = app.snapshot().unwrap();
        assert_eq!(imported.catalog.items.len(), 1);
        let merged = imported.catalog.items.values().next().unwrap();
        assert!(
            merged
                .provenance
                .iter()
                .any(|value| value == "tempo1:capture:manual")
        );
        let first = study(&mut app, json!({"command":"next"}));
        assert_eq!(first["activity"], "test");
        assert!(first["feedback"].is_null());
        let mut advance = activity_command("answer", &first, "first");
        advance["answer"] = json!({"uci":"g1h1"});
        let second = study(&mut app, advance);
        assert_eq!(second["prompt"]["decision_index"], 1);
        assert!(second["feedback"].get("tactic_explanation").is_none());
        let shown = study(&mut app, activity_command("give_up", &second, "show"));
        assert_eq!(shown["feedback"]["correction_uci"], "e2b5");
        assert!(shown["feedback"].get("tactic_explanation").is_none());
        let mut miss = activity_command("answer", &shown, "wrong");
        miss["answer"] = json!({"uci":"e2d3"});
        let missed = study(&mut app, miss.clone());
        assert_eq!(missed["feedback"]["correction_uci"], "e2b5");
        assert_eq!(missed["feedback"]["tactic_explanation"]["kind"], "pin");
        let mut correction = activity_command("answer", &missed, "finish");
        correction["answer"] = json!({"uci":"e2b5"});
        let completed = study(&mut app, correction);
        assert_eq!(completed["status"], "complete");
        assert_eq!(completed["assessment"], "again");
        assert!(completed["feedback"].get("tactic_explanation").is_none());
        let durable = app.snapshot().unwrap();
        assert_eq!(durable.assessments.len(), 1);
        assert_eq!(study(&mut app, miss), missed);
        assert_eq!(app.snapshot().unwrap(), durable);
    }
}

#[test]
fn captures_interrupt_only_the_next_selection_and_cost_no_introductions() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("personal-captures.sqlite");
    let day = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits {
        repertoire: 0,
        tactics: 0,
    };
    let mut app = open(&db, day.clone(), limits, 0);
    let fen = "4k3/8/8/8/8/8/4P3/4K3 w - - 0 1";
    let legacy = [
        snippet("Legacy 1", fen, "Kd2"),
        snippet("Legacy 2", fen, "Kf2"),
    ]
    .concat();
    transfer_tactics(&mut app, &legacy, &[0, 1], "tempo1:capture:legacy-fixture");
    let before = app.snapshot().unwrap();
    let dry = app
        .backfill_personal_tactics(&Default::default(), false)
        .unwrap();
    assert_eq!(dry["introduced"], 2);
    assert_eq!(app.snapshot().unwrap(), before);
    assert_eq!(
        app.backfill_personal_tactics(&Default::default(), true)
            .unwrap()["status"],
        "applied"
    );
    let issued = study(&mut app, json!({"command":"next"}));
    let captured = snippet("New capture", fen, "e4");
    transfer_tactics(&mut app, &captured, &[0], "test catalog");
    let (_, saved) = save_tactics(&mut app, &captured, json!([]), vec![0]);
    assert_eq!(saved["ready_for_review"], 1);
    assert_eq!(app.snapshot().unwrap().catalog.items.len(), 3);
    drop(app);

    let mut app = open(&db, day.clone(), limits, 1);
    app.set_local_tactic_preferences(0, Some(tempo::TacticThemes::starter()));
    assert_eq!(study(&mut app, json!({"command":"next"})), issued);
    let skip = activity_command("skip", &issued, "skip-legacy");
    assert_eq!(study(&mut app, skip)["status"], "abandoned");
    let next = study(&mut app, json!({"command":"next"}));
    let target = TargetId::from(next["target_id"].as_str().unwrap());
    let before_grade = app.snapshot().unwrap();
    assert_eq!(
        before_grade.catalog.items[&target].definition.decisions[0]
            .expected
            .uci,
        "e2e4"
    );
    assert!(before_grade.assessments.is_empty());
    let mut answer = activity_command("answer", &next, "solve-capture");
    answer["answer"] = json!({"uci":"e2e4"});
    assert_eq!(study(&mut app, answer)["assessment"], "good");
    app.set_local_tactic_preferences(1, Some(tempo::TacticThemes::starter()));
    assert_eq!(
        study(&mut app, json!({"command":"study_status"}))["tactics_remaining"],
        1
    );
    let graded = app.snapshot().unwrap();
    day.store(20_001, Ordering::Relaxed);
    assert_eq!(
        app.backfill_personal_tactics(&Default::default(), true)
            .unwrap()["status"],
        "already_applied"
    );
    assert_eq!(app.snapshot().unwrap(), graded);
}

#[test]
fn public_gap_list_reads_forecast_only_rows_and_opens_the_signed_editor() {
    use tempo_chess::{PositionCanonicalizer, StandardPositionCanonicalizer};
    use tempo_store::CoverageSummary;

    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("forecast-response.sqlite");
    let mut app = open(
        &database,
        Arc::new(AtomicI32::new(20_000)),
        IntroductionLimits::default(),
        0,
    );
    let (_, saved) = preview_and_save(&mut app, "1. e4 e5 (1... c5) 2. Nf3 *", "Book", None);
    let id = saved["repertoires"][0]["id"].as_str().unwrap();
    let dataset = "opening-proof".into();
    let mut store = SqliteStore::open(&database).unwrap();
    let book = store.coverage_book(&dataset, id).unwrap().unwrap();
    let doc = store.repertoire_document(id).unwrap();
    let empty = store.coverage_start(&dataset, id).unwrap();
    let initial = store
        .save_coverage_start(&dataset, id, doc.revision, empty.revision, &["e2e4".into()])
        .unwrap();
    assert_eq!(
        store
            .save_coverage_start(&dataset, id, doc.revision, empty.revision, &["e2e4".into()])
            .unwrap(),
        initial
    );
    let position = StandardPositionCanonicalizer
        .canonicalize("rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1")
        .unwrap();
    store
        .publish_coverage(
            &dataset,
            id,
            &book.input_key,
            None,
            &CoverageSummary {
                starting_line: initial.clone(),
                starting_san: vec!["e4".into()],
                request_id: "coverage-contract".into(),
                through_move: 2,
                target_percent: 90,
                covered: 0.75,
                unresolved: 0.0,
                positions: 1,
                explorer_positions: 0,
                maia_positions: 1,
                scanned_at_ms: 1,
            },
            &[(position, "c7c5".into(), 0.25)],
        )
        .unwrap();
    let page = app.opponent_gaps(0).unwrap();
    let [gap] = page["gaps"].as_array().unwrap().as_slice() else {
        panic!("{page}")
    };
    assert_eq!(gap["games"], 0);
    assert_eq!(gap["gap_kind"], "forecast");
    assert_eq!(gap["forecast"]["probability"], 0.25);
    let address = json!({
        "dataset_id": dataset, "repertoire_id":id, "position":gap["position"],
        "actual_move":gap["actual_move"], "document_revision":gap["document_revision"],
        "gap_kind":gap["gap_kind"], "response":"g1f3",
    });
    let draft = app
        .game_gap_draft(serde_json::from_value(address.clone()).unwrap())
        .unwrap();
    assert!(draft["save_token"].is_string(), "{draft}");
    let save = |draft: &Value| {
        json!({
            "command":"save", "dataset_id":dataset, "repertoire_id":id,
            "name":"Book", "learner_color":"white", "pgn":draft["draft"]["pgn"], "token":draft["save_token"],
        })
    };
    let unaccepted = save(&draft);
    let suspended = content(&mut app, activity_change(id, "forecast-suspend", "suspended"));
    assert_eq!(suspended["status"], "activity_set", "{suspended}");
    let suspended_state = app.snapshot().unwrap();
    assert_eq!(
        content(&mut app, unaccepted.clone())["code"],
        "stale_coverage"
    );
    assert_eq!(app.snapshot().unwrap(), suspended_state);
    let resumed = content(&mut app, activity_change(id, "forecast-resume", "active"));
    assert_eq!(resumed["status"], "activity_set", "{resumed}");
    let before = app.snapshot().unwrap();
    let cleared = store
        .save_coverage_start(&dataset, id, doc.revision, initial.revision, &[])
        .unwrap();
    let restored = store
        .save_coverage_start(
            &dataset,
            id,
            doc.revision,
            cleared.revision,
            &["e2e4".into()],
        )
        .unwrap();
    assert_ne!(restored.revision, initial.revision);
    assert!(
        app.opponent_gaps(0).unwrap()["gaps"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let saved = store.coverage_summary(&dataset, id).unwrap().unwrap();
    assert!(saved.stale);
    assert!(
        store
            .publish_coverage(
                &dataset,
                id,
                &book.input_key,
                Some("coverage-contract"),
                &saved.summary,
                &[]
            )
            .is_err()
    );
    assert_eq!(content(&mut app, unaccepted)["code"], "stale_coverage");
    assert_eq!(app.snapshot().unwrap(), before);
    let mut summary = saved.summary;
    summary.starting_line = restored.clone();
    summary.request_id = "fresh-coverage".into();
    let position = serde_json::from_value(address["position"].clone()).unwrap();
    store
        .publish_coverage(
            &dataset,
            id,
            &book.input_key,
            Some("coverage-contract"),
            &summary,
            &[(position, "c7c5".into(), 0.25)],
        )
        .unwrap();
    let draft = app
        .game_gap_draft(serde_json::from_value(address).unwrap())
        .unwrap();
    let accepted = save(&draft);
    let response = content(&mut app, accepted.clone());
    assert_eq!(response["status"], "repertoires", "{response}");
    let updated = store.repertoire_document(id).unwrap();
    store
        .save_coverage_start(&dataset, id, updated.revision, restored.revision, &[])
        .unwrap();
    let after = app.snapshot().unwrap();
    assert_eq!(content(&mut app, accepted), response);
    assert_eq!(app.snapshot().unwrap(), after);
}

#[test]
fn gap_preview_signs_only_responses_that_preserve_existing_answers_and_learning() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = open(
        &directory.path().join("gap-response.sqlite"),
        Arc::new(AtomicI32::new(20_000)),
        IntroductionLimits::default(),
        0,
    );
    let (_, book) = preview_and_save(
        &mut app,
        "1. e4 e5 (1... d5 2. exd5) 2. Nf3 *",
        "Book",
        None,
    );
    let id = book["repertoires"][0]["id"].as_str().unwrap();
    for index in 0..3 {
        let lesson = study(&mut app, json!({"command":"next"}));
        assert_eq!(lesson["activity"], "teach", "{lesson}");
        let mut ack = activity_command("acknowledge", &lesson, &format!("gap-teach-{index}"));
        ack["answer"] = json!({"uci":lesson["lesson"]["move_uci"]});
        assert_eq!(study(&mut app, ack)["status"], "complete");
    }
    preview_and_save(&mut app, SHORT, "Book", Some(id));
    date_only_import(&mut app, "scandinavian-gap", "2024.10.06", "1. e4 d5 2. d4");
    let gaps = app.opponent_gaps(0).unwrap();
    let gap = &gaps["gaps"][0];
    assert_eq!(gap["actual_san"], "d5");
    let address = json!({
        "dataset_id":"opening-proof", "repertoire_id":id, "position":gap["position"],
        "actual_move":gap["actual_move"], "document_revision":gap["document_revision"],
    });
    let draft = app
        .game_gap_draft(serde_json::from_value(address.clone()).unwrap())
        .unwrap();
    assert_eq!(draft["response_move_labels"]["e4d5"], "exd5");
    assert!(draft["save_token"].is_null());
    let mut request = address;
    let revision = draft["content_revision"].as_u64().unwrap();
    request["base_revision"] = json!(revision - 1);
    request["response"] = json!("e4d5");
    let rejected = app.game_gap_draft(serde_json::from_value(request.clone()).unwrap());
    assert_eq!(rejected.unwrap_err().code, "stale_draft");

    request["base_revision"] = json!(revision);
    request["response"] = json!("d2d4");
    let before = app.snapshot().unwrap();
    let rejected = app.game_gap_draft(serde_json::from_value(request.clone()).unwrap());
    assert_eq!(rejected.unwrap_err().code, "unsafe_gap_response");
    assert_eq!(app.snapshot().unwrap(), before);

    request["response"] = json!("e4d5");
    let proposed = app
        .game_gap_draft(serde_json::from_value(request).unwrap())
        .unwrap();
    assert_eq!(app.snapshot().unwrap(), before);
    let mut save = book_request(
        "save",
        proposed["draft"]["pgn"].as_str().unwrap(),
        "Book",
        Some(id),
    );
    save["token"] = proposed["save_token"].clone();
    let receipt = content(&mut app, save.clone());
    assert_eq!(receipt["status"], "repertoires", "{receipt}");
    let accepted = app.snapshot().unwrap();
    assert_eq!(before.learning, accepted.learning);
    assert_eq!(before.assessments, accepted.assessments);
    assert_eq!(content(&mut app, save), receipt);
    assert_eq!(app.snapshot().unwrap(), accepted);
}

#[test]
fn current_pgn_read_and_edit_keep_the_full_document_and_its_content_fence() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("current-pgn.sqlite");
    let day = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits::default();
    let mut app = open(&db, day.clone(), limits, 0);
    let original = "1. e4 e5 2. Nf3 *\n";
    let authored = "[Event \"Annotated\"]\n\n1. e4 e5 2. Nf3 *\n";
    let (_, created) = preview_and_save(&mut app, original, "Repertoire", None);
    let id = created["repertoires"][0]["id"].as_str().unwrap();
    let read = json!({"command":"read_document", "dataset_id":"opening-proof", "repertoire_id":id});
    let before = app.snapshot().unwrap();
    let loaded = content(&mut app, read.clone());
    assert_eq!(
        loaded["document"],
        json!({"status":"available","pgn": original})
    );
    assert_eq!(app.snapshot().unwrap(), before);
    assert!(created["repertoires"][0].get("pgn").is_none());

    let mut edit = book_request("edit_preview", authored, "Repertoire", Some(id));
    edit["base_revision"] = loaded["revision"].clone();
    let preview = content(&mut app, edit.clone());
    assert_eq!(preview["status"], "preview", "{preview}");
    assert_eq!(preview["changed"], 0);
    assert_eq!(preview["removed"], 0);
    let activity = study(&mut app, json!({"command":"next"}));
    let before_read = app.snapshot().unwrap();
    assert_eq!(content(&mut app, read.clone()), loaded);
    assert_eq!(app.snapshot().unwrap(), before_read);
    let mut ack = activity_command("acknowledge", &activity, "learn-between-preview-and-save");
    ack["answer"] = json!({"uci":"e2e4"});
    assert_eq!(study(&mut app, ack)["status"], "complete");
    let learned = app.snapshot().unwrap();

    let mut save = book_request("save", authored, "Repertoire", Some(id));
    save["token"] = preview["token"].clone();
    let receipt = content(&mut app, save.clone());
    assert_eq!(receipt["status"], "repertoires", "{receipt}");
    assert_eq!(app.snapshot().unwrap().learning, learned.learning);
    assert_eq!(content(&mut app, edit)["code"], "stale_draft");
    let annotated = content(&mut app, read.clone());
    assert_eq!(annotated["document"]["pgn"], authored);
    let mut stale = book_request("edit_preview", "1. d4 d5 *", "Repertoire", Some(id));
    stale["base_revision"] = annotated["revision"].clone();
    let stale_preview = content(&mut app, stale);
    let mut stale_save = book_request("save", "1. d4 d5 *", "Repertoire", Some(id));
    stale_save["token"] = stale_preview["token"].clone();

    let annotated_catalog = app.snapshot().unwrap().catalog;
    preview_and_save(&mut app, original, "Repertoire", Some(id));
    let restored = app.snapshot().unwrap();
    assert_eq!(restored.catalog, annotated_catalog);
    assert_eq!(restored.learning, learned.learning);
    let returned = content(&mut app, read.clone());
    assert_eq!(returned["document"]["pgn"], original);
    assert_eq!(
        returned["revision"].as_u64().unwrap(),
        annotated["revision"].as_u64().unwrap() + 1
    );
    assert_eq!(content(&mut app, stale_save)["code"], "stale_preview");
    assert_eq!(app.snapshot().unwrap(), restored);

    content(
        &mut app,
        json!({"command":"rename","dataset_id":"opening-proof","repertoire_id":id,"name":"Renamed"}),
    );
    drop(app);
    let mut app = open(&db, day, limits, 1);
    let latest = content(&mut app, read.clone());
    assert_eq!(latest["name"], "Renamed");
    assert_eq!(latest["document"]["pgn"], original);
    let before_replay = app.snapshot().unwrap();
    assert_eq!(content(&mut app, save), receipt);
    assert_eq!(content(&mut app, read), latest);
    assert_eq!(app.snapshot().unwrap(), before_replay);
}

#[test]
fn game_import_is_answer_safe_and_preserves_active_work_across_restart() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("games.sqlite");
    let today = Arc::new(AtomicI32::new(20_000));
    let mut app = open(&db, today.clone(), IntroductionLimits::default(), 0);
    let (_, saved) = preview_and_save(&mut app, SHORT, "White", None);
    let repertoire = saved["repertoires"][0]["id"].as_str().unwrap();
    let command = json!({
        "repertoire_id": repertoire, "learner":"learner", "command_id":"first-import", "dataset_id":"opening-proof",
        "pgn":"[White \"Learner\"]\n[Black \"Other\"]\n1. d4 (1. e4 e5) d5 1-0\n\n[White \"Learner\"]\n[Black \"Other\"]\n1. e9 1-0",
    });
    let active = study(&mut app, json!({"command":"next"}));
    let before = app.snapshot().unwrap();
    let report = app
        .import_game_pgn(serde_json::from_value(command.clone()).unwrap())
        .unwrap();
    assert_eq!(
        (report.fetched, report.new_games, report.deviations),
        (2, 1, 1)
    );
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.results[0].summary.first_deviation_ply, Some(1));
    let public = serde_json::to_string(&report).unwrap();
    for secret in ["e2e4", "d2d4", "prescribed", "played"] {
        assert!(!public.contains(secret), "{public}");
    }
    let after = app.snapshot().unwrap();
    assert_eq!(after.attempts, before.attempts);
    assert_eq!(after.assessments, before.assessments);
    assert_eq!(
        after
            .learning
            .values()
            .filter(|state| state.correction.is_some())
            .count(),
        1
    );
    assert_eq!(
        SqliteStore::open(&db)
            .unwrap()
            .game_failures(&after.dataset)
            .unwrap()
            .len(),
        1
    );
    drop(app);
    let mut app = open(&db, today, IntroductionLimits::default(), 1);
    let replay = app
        .import_game_pgn(serde_json::from_value(command.clone()).unwrap())
        .unwrap();
    assert_eq!(
        serde_json::to_value(&replay).unwrap(),
        serde_json::to_value(&report).unwrap()
    );
    let mut command = command;
    command["command_id"] = json!("duplicate-import");
    let report = app
        .import_game_pgn(serde_json::from_value(command).unwrap())
        .unwrap();
    assert_eq!((report.new_games, report.duplicates), (0, 1));
    assert_eq!(
        app.game_history().unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(study(&mut app, json!({"command":"next"})), active);
    assert_eq!(app.snapshot().unwrap(), after);
}

fn dated_game(app: &mut Tempo, repertoire: &str, date: &str, play: &str, time: &str) -> Value {
    let pgn = format!(
        "[White \"Learner\"]\n[Black \"Other\"]\n[UTCDate \"{date}\"]\n[UTCTime \"{time}\"]\n[UTCEndDate \"{date}\"]\n[UTCEndTime \"23:00:00\"]\n{play} 1-0"
    );
    static IDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let request = json!({"repertoire_id":repertoire,"learner":"learner","pgn":pgn,
        "dataset_id":app.snapshot().unwrap().dataset,
        "command_id":format!("fixture-{}", IDS.fetch_add(1, Ordering::Relaxed))});
    serde_json::to_value(
        app.import_game_pgn(serde_json::from_value(request).unwrap())
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn reset_progress_scopes_study_history_and_games_and_replays_after_new_learning() {
    use tempo_chess::ContentKind;
    use tempo_learning::LearningState;

    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("reset.sqlite");
    let day = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits::default();
    let mut app = open(&db, day.clone(), limits, 0);
    let (_, saved) = preview_and_save(&mut app, SHORT, "London", None);
    let london = saved["repertoires"][0]["id"].as_str().unwrap();
    for index in 0..2 {
        let lesson = study(&mut app, json!({"command":"next"}));
        let mut ack = activity_command("acknowledge", &lesson, &format!("london-{index}"));
        ack["answer"] = json!({"uci":lesson["lesson"]["move_uci"]});
        assert_eq!(study(&mut app, ack)["status"], "complete");
    }
    preview_and_save(&mut app, "1. e4 *", "London", Some(london));
    let (_, saved) = preview_and_save(&mut app, "1. d4 *", "Other", None);
    let other = saved["repertoires"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "Other")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let lesson = study(&mut app, json!({"command":"next"}));
    let mut other_ack = activity_command("acknowledge", &lesson, "other-lesson");
    other_ack["answer"] = json!({"uci":"d2d4"});
    let other_result = study(&mut app, other_ack.clone());
    assert_eq!(other_result["status"], "complete");

    day.store(20_001, Ordering::Relaxed);
    let old_game = json!({
        "repertoire_id":london, "learner":"learner", "command_id":"london-game",
        "dataset_id":"opening-proof",
        "pgn":"[White \"Learner\"]\n[Black \"Other\"]\n1. d4 d5 1-0",
    });
    let import = |app: &mut Tempo, request: Value| {
        serde_json::to_value(
            app.import_game_pgn(serde_json::from_value(request).unwrap())
                .unwrap(),
        )
        .unwrap()
    };
    let original_game = import(&mut app, old_game.clone());
    let mut london_test = None;
    for index in 0..2 {
        let test = study(&mut app, json!({"command":"next"}));
        let mut answer = activity_command("answer", &test, &format!("test-{index}"));
        answer["answer"] =
            json!({"uci":if test["exercise_name"] == "London" {"e2e4"} else {"d2d4"}});
        assert_eq!(study(&mut app, answer.clone())["assessment"], "good");
        if test["exercise_name"] == "London" {
            london_test = Some(answer);
        }
    }
    let london_test = london_test.unwrap();
    let before = app.snapshot().unwrap();
    let read =
        json!({"command":"read_document", "dataset_id":"opening-proof", "repertoire_id":london});
    let document = content(&mut app, read.clone());
    let reset = json!({"command":"reset_progress", "dataset_id":"opening-proof",
        "repertoire_id":london, "command_id":"reset-london"});
    assert_eq!(
        content(
            &mut app,
            json!({
                "command":"reset_progress", "dataset_id":"other-dataset", "repertoire_id":london,
                "command_id":"wrong-dataset",
            })
        )["code"],
        "stale_dataset"
    );
    assert_eq!(app.snapshot().unwrap(), before);
    let accepted = content(&mut app, reset.clone());
    assert_eq!(accepted["status"], "progress_reset", "{accepted}");
    assert_eq!(content(&mut app, read), document);
    let after = app.snapshot().unwrap();
    let targets: Vec<_> = before.catalog.items.values().filter(|item|
        matches!(&item.definition.kind, ContentKind::Repertoire { repertoire, .. } if repertoire == london))
        .map(|item| &item.target).collect();
    assert_eq!(targets.len(), 2);
    assert_eq!(after.catalog, before.catalog);
    assert_eq!(after.content_history, before.content_history);
    for (target, state) in &after.learning {
        if targets.contains(&target) {
            assert_eq!(*state, LearningState::default());
        } else {
            assert_eq!(state, &before.learning[target]);
        }
    }
    assert!(
        after
            .attempts
            .values()
            .all(|attempt| !targets.contains(&&attempt.attempt.target))
    );
    assert_eq!(after.assessments.len(), 1);
    assert!(!targets.contains(&&after.assessments[0].target));
    assert_eq!(study(&mut app, london_test)["code"], "unknown_attempt");
    assert_eq!(study(&mut app, other_ack), other_result);
    let mut conflicting = reset.clone();
    conflicting["repertoire_id"] = json!(other);
    assert_eq!(content(&mut app, conflicting)["code"], "command_conflict");

    assert_eq!(import(&mut app, old_game.clone()), original_game);
    let mut duplicate = old_game;
    duplicate["command_id"] = json!("london-game-duplicate");
    assert_eq!(
        import(&mut app, duplicate)["results"][0]["effect"],
        "recorded_only"
    );
    assert_eq!(
        app.game_history().unwrap()["results"][0]["effect"],
        "recorded_only"
    );
    assert_eq!(app.snapshot().unwrap(), after);

    let lesson = study(&mut app, json!({"command":"next"}));
    assert_eq!(lesson["exercise_name"], "London");
    assert_eq!(lesson["lesson"]["move_uci"], "e2e4");
    let mut ack = activity_command("acknowledge", &lesson, "learn-london-again");
    ack["answer"] = json!({"uci":"e2e4"});
    assert_eq!(study(&mut app, ack)["status"], "complete");
    let learned_again = app.snapshot().unwrap();
    drop(app);
    let mut app = open(&db, day.clone(), limits, 1);
    assert_eq!(content(&mut app, reset), accepted);
    assert_eq!(app.snapshot().unwrap(), learned_again);

    day.store(20_002, Ordering::Relaxed);
    let new_game = dated_game(&mut app, london, "2024.10.05", "1. d4 d5", "12:00:00");
    assert_eq!(new_game["results"][0]["effect"], "recorded_only");
}

fn date_only_import(app: &mut Tempo, id: &str, date: &str, moves: &str) -> Value {
    let request = json!({
        "learner": "learner", "command_id": id, "dataset_id": app.snapshot().unwrap().dataset,
        "pgn": format!("[White \"Learner\"]\n[Black \"Other\"]\n[Date \"{date}\"]\n{moves} 1-0")
    });
    serde_json::to_value(
        app.import_game_pgn(serde_json::from_value(request).unwrap())
            .unwrap(),
    )
    .unwrap()
}

fn play_activity(app: &mut Tempo, activity: &Value, command: &str, id: &str, uci: &str) -> Value {
    let mut answer = activity_command(command, activity, id);
    answer["answer"] = json!({"uci": uci});
    study(app, answer)
}

fn repair_context(app: &Tempo, activity: &Value) -> Value {
    let dataset = activity["dataset_id"].as_str().unwrap().into();
    let attempt = activity["attempt_id"].as_str().unwrap().into();
    app.study_game_context(&dataset, &attempt).unwrap()["context"].clone()
}

#[test]
fn study_progress_counts_pending_repairs_including_skips() {
    let directory = tempfile::tempdir().unwrap();
    let day = Arc::new(AtomicI32::new(20_004));
    let mut app = open(
        &directory.path().join("repair-progress.sqlite"),
        day.clone(),
        IntroductionLimits {
            repertoire: 0,
            tactics: 0,
        },
        0,
    );
    let (_, book) = preview_and_save(&mut app, "1. e4 e5 2. Nf3 *", "Opening", None);
    let progress = |app: &Tempo| app.study_progress(None).unwrap().unwrap();

    date_only_import(&mut app, "root-miss", "2024.10.06", "1. d4 d5");
    date_only_import(&mut app, "later-miss", "2024.10.06", "1. e4 e5 2. Bc4");
    assert_eq!(progress(&app).repairs_remaining, 2);
    let first = study(&mut app, json!({"command":"next"}));
    let before_skip = progress(&app);
    assert_eq!(before_skip.repairs_remaining, 2);
    assert!(
        app.study_progress(Some(&before_skip.stamp))
            .unwrap()
            .is_none()
    );
    let skipped = study(&mut app, activity_command("skip", &first, "skip-repair"));
    assert_eq!(skipped["status"], "abandoned");
    let second = study(&mut app, json!({"command":"next"}));
    assert_ne!(first["target_id"], second["target_id"]);
    assert_eq!(progress(&app).repairs_remaining, 2);

    let answer = second["lesson"]["move_uci"].as_str().unwrap();
    let saved = play_activity(&mut app, &second, "acknowledge", "save-repair", answer);
    assert_eq!(saved["status"], "complete");
    assert_eq!(
        app.study_progress(Some(&before_skip.stamp))
            .unwrap()
            .unwrap()
            .repairs_remaining,
        1
    );
    assert_eq!(
        play_activity(&mut app, &second, "acknowledge", "save-repair", answer),
        saved
    );
    assert_eq!(progress(&app).repairs_remaining, 1);
    day.store(20_005, Ordering::Relaxed);
    assert_eq!(progress(&app).due, 1);

    date_only_import(&mut app, "new-root-miss", "2024.10.07", "1. c4 e5");
    date_only_import(&mut app, "new-later-miss", "2024.10.07", "1. e4 e5 2. Nc3");
    assert_eq!(progress(&app).repairs_remaining, 2);
    let id = book["repertoires"][0]["id"].as_str().unwrap();
    preview_and_save(&mut app, "1. d4 *", "Opening", Some(id));
    assert_eq!(progress(&app).repairs_remaining, 0);
}

#[test]
fn repair_context_follows_the_issued_miss_and_preserves_study() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("game-context.sqlite");
    let day = Arc::new(AtomicI32::new(20_004));
    let limits = IntroductionLimits {
        repertoire: 0,
        tactics: 0,
    };
    let mut app = open(&db, day.clone(), limits, 0);
    let (_, book) = preview_and_save(&mut app, "1. e4 e5 2. Nf3 *", "London", None);
    let dataset = "opening-proof";
    date_only_import(&mut app, "first", "2024.10.06", "1. e4 e5 2. Bc4 Nc6");
    let first = study(&mut app, json!({"command":"next"}));
    assert_eq!(first["lesson"]["move_san"], "Nf3");
    let before = app.snapshot().unwrap();
    let context = repair_context(&app, &first);
    assert_eq!(context["played_san"], "Bc4");
    assert_eq!(context["played_uci"], "f1c4");
    assert_eq!(context["url"], Value::Null);
    let moves = context["line"]["moves"].as_array().unwrap();
    assert_eq!(
        moves
            .iter()
            .map(|m| m["notation"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["1. e4", "1... e5", "2. Bc4"]
    );
    assert!(
        moves.last().unwrap()["fen"]
            .as_str()
            .unwrap()
            .contains("/2B1P3/")
    );
    assert_eq!(app.snapshot().unwrap(), before);
    let attempt = first["attempt_id"].as_str().unwrap().into();
    assert_eq!(
        app.study_game_context(&"other".into(), &attempt).unwrap()["context"],
        Value::Null
    );

    // A later Game Again at the same target cannot retarget an issued card.
    day.store(20_005, Ordering::Relaxed);
    date_only_import(&mut app, "second", "2024.10.07", "1. e4 e5 2. Nc3 Nc6");
    drop(app);
    let mut app = open(&db, day.clone(), limits, 1);
    assert_eq!(repair_context(&app, &first), context);
    let saved = play_activity(&mut app, &first, "acknowledge", "save-first", "g1f3");
    assert_eq!(saved["status"], "complete");
    assert_eq!(repair_context(&app, &first), Value::Null);
    let second = study(&mut app, json!({"command":"next"}));
    assert_eq!(repair_context(&app, &second)["played_san"], "Nc3");

    let archive = app.export(&tempo::JsonSnapshotCodec).unwrap();
    let mut restored = open(
        &directory.path().join("restored.sqlite"),
        day.clone(),
        limits,
        0,
    );
    restored
        .restore("restored".into(), &archive, &tempo::JsonSnapshotCodec)
        .unwrap();
    let restored_repair = study(&mut restored, json!({"command":"next"}));
    assert_eq!(restored_repair["corrective_repair"], true);
    assert_eq!(repair_context(&restored, &restored_repair), Value::Null);

    assert_eq!(
        play_activity(&mut app, &second, "acknowledge", "save-second", "g1f3")["assessment"],
        Value::Null
    );
    let progress = app.study_progress(None).unwrap().unwrap();
    assert_eq!(
        (progress.openings_introduced, progress.openings_limit),
        (0, 0)
    );
    day.store(20_006, Ordering::Relaxed);
    let blind = study(&mut app, json!({"command":"next"}));
    assert_eq!(blind["activity"], "test");
    assert_eq!(repair_context(&app, &blind), Value::Null);

    let reset = content(
        &mut app,
        json!({"command":"reset_progress", "dataset_id":dataset,
        "repertoire_id":book["repertoires"][0]["id"], "command_id":"reset-context"}),
    );
    assert_eq!(reset["status"], "progress_reset");
    day.store(20_007, Ordering::Relaxed);
    let game = date_only_import(&mut app, "after-reset", "2024.10.11", "1. e4 e5 2. d4 Nc6");
    assert_eq!(game["results"][0]["effect"], "again_applied");
    let fresh = study(&mut app, json!({"command":"next"}));
    assert_eq!(repair_context(&app, &fresh)["played_san"], "d4");
    assert_eq!(repair_context(&app, &first), Value::Null);
}

#[test]
fn repair_context_omits_an_ambiguous_repeated_game_position() {
    let directory = tempfile::tempdir().unwrap();
    let day = Arc::new(AtomicI32::new(20_004));
    let mut app = open(
        &directory.path().join("game-context-repeat.sqlite"),
        day,
        IntroductionLimits::default(),
        0,
    );
    preview_and_save(&mut app, "1. e4 e5 2. Nf3 *", "Book", None);
    let report = date_only_import(
        &mut app,
        "repeat",
        "2024.10.06",
        "1. e4 e5 2. Bc4 Nc6 3. Bf1 Nb8 4. Bc4 Nc6",
    );
    assert_eq!(report["deviations"], 2);
    assert_eq!(report["results"][0]["effect"], "again_applied");
    let repair = study(&mut app, json!({"command":"next"}));
    assert_eq!(repair["corrective_repair"], true);
    assert_eq!(repair_context(&app, &repair), Value::Null);
}

#[test]
fn date_only_game_again_restarts_exact_position_repair_and_three_day_recall() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("daily-games.sqlite");
    let today = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits::default();
    let mut app = open(&db, today.clone(), limits, 0);
    preview_and_save(&mut app, "1. e4 *", "White", None);
    let lesson = study(&mut app, json!({"command":"next"}));
    let target = TargetId::from(lesson["target_id"].as_str().unwrap());
    assert_eq!(
        play_activity(&mut app, &lesson, "acknowledge", "teach", "e2e4")["assessment"],
        Value::Null
    );
    today.store(20_001, Ordering::Relaxed);
    let first = study(&mut app, json!({"command":"next"}));
    assert_eq!(
        play_activity(&mut app, &first, "answer", "prior-good", "e2e4")["assessment"],
        "good"
    );
    let memory = app.snapshot().unwrap().learning[&target]
        .scheduler_memory
        .clone();
    let report = date_only_import(&mut app, "first-game", "2024.10.02", "1. d4 d5");
    assert_eq!(report["results"][0]["effect"], "again_applied", "{report}");
    assert_eq!(
        date_only_import(&mut app, "first-game", "2024.10.02", "1. d4 d5"),
        report
    );
    assert_eq!(
        date_only_import(&mut app, "duplicate", "2024.10.02", "1. d4 d5")["results"][0]["effect"],
        "already_addressed"
    );
    let state = app.snapshot().unwrap().learning[&target].clone();
    assert_ne!(state.scheduler_memory, memory);
    assert_eq!(state.last_graded_day, Some(StudyDay(20_001)));
    assert_eq!(state.correction.as_ref().unwrap().clean_recalls, 0);

    let repair = study(&mut app, json!({"command":"next"}));
    assert_eq!(repair["corrective_repair"], true, "{repair}");
    assert_eq!(repair["target_id"], target.0);
    assert_eq!(
        play_activity(&mut app, &repair, "acknowledge", "repair", "e2e4")["assessment"],
        Value::Null
    );
    assert_eq!(
        study(&mut app, json!({"command":"next"}))["corrective_recalls_remaining"],
        3
    );
    today.store(20_002, Ordering::Relaxed);
    let recall = study(&mut app, json!({"command":"next"}));
    assert_eq!(recall["activity"], "test");
    assert!(recall.get("lesson").is_none());
    assert_eq!(
        play_activity(&mut app, &recall, "answer", "first-recall", "e2e4")["assessment"],
        "good"
    );
    assert_eq!(
        study(&mut app, json!({"command":"next"}))["corrective_recalls_remaining"],
        2
    );
    drop(app);
    let mut app = open(&db, today.clone(), limits, 1);

    let package = app.export(&tempo::JsonSnapshotCodec).unwrap();
    let mut restored = open(
        &directory.path().join("restored.sqlite"),
        today.clone(),
        limits,
        0,
    );
    restored
        .restore("restored".into(), &package, &tempo::JsonSnapshotCodec)
        .unwrap();
    assert_eq!(
        date_only_import(&mut restored, "restored-game", "2024.10.02", "1. d4 d5")["results"][0]["effect"],
        "already_addressed"
    );
    assert_eq!(
        restored.snapshot().unwrap().learning[&target],
        app.snapshot().unwrap().learning[&target]
    );

    today.store(20_003, Ordering::Relaxed);
    let older = study(&mut app, json!({"command":"next"}));
    let second = date_only_import(&mut app, "second-game", "2024.10.06", "1. c4 e5");
    assert_eq!(second["results"][0]["effect"], "again_applied");
    assert_eq!(
        study(&mut app, json!({"command":"next"}))["attempt_id"],
        older["attempt_id"]
    );
    let newer = app.snapshot().unwrap().learning[&target].clone();
    assert_eq!(
        play_activity(&mut app, &older, "answer", "old-good", "e2e4")["status"],
        "complete"
    );
    assert_eq!(app.snapshot().unwrap().learning[&target], newer);
    let repair = study(&mut app, json!({"command":"next"}));
    assert_eq!(
        play_activity(&mut app, &repair, "acknowledge", "second-repair", "e2e4")["status"],
        "complete"
    );

    today.store(20_004, Ordering::Relaxed);
    let test = study(&mut app, json!({"command":"next"}));
    let miss = play_activity(&mut app, &test, "answer", "card-miss", "d2d4");
    assert_eq!(miss["assessment"], "again");
    assert_eq!(
        play_activity(&mut app, &miss, "answer", "inline-repair", "e2e4")["assessment"],
        "again"
    );
    let reset = app.snapshot().unwrap().learning[&target].clone();
    assert_eq!(reset.miss_revision.0, newer.miss_revision.0 + 1);
    assert_eq!(
        reset.correction.unwrap().next_recall_day,
        Some(StudyDay(20_005))
    );
    for count in 0..3 {
        today.store(20_005 + count, Ordering::Relaxed);
        let recall = study(&mut app, json!({"command":"next"}));
        assert_eq!(
            play_activity(
                &mut app,
                &recall,
                "answer",
                &format!("recall-{count}"),
                "e2e4"
            )["assessment"],
            "good"
        );
        let state = app.snapshot().unwrap().learning[&target].correction.clone();
        assert_eq!(
            state.map(|correction| correction.clean_recalls),
            (count < 2).then_some(count as u8 + 1)
        );
    }
    assert_eq!(
        SqliteStore::open(&db)
            .unwrap()
            .game_failures(&app.snapshot().unwrap().dataset)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn combined_books_match_italian_play_and_repair_an_unstudied_position_at_zero_quota() {
    let directory = tempfile::tempdir().unwrap();
    let today = Arc::new(AtomicI32::new(20_004));
    let db = directory.path().join("frontier.sqlite");
    let mut app = open(
        &db,
        today,
        IntroductionLimits {
            repertoire: 0,
            tactics: 0,
        },
        0,
    );
    preview_and_save(&mut app, "1. d4 d5 2. Bf4 *", "London", None);
    preview_and_save(&mut app, "1. e4 e5 2. Nf3 Nc6 3. Bb5 *", "Ruy Lopez", None);
    preview_and_save(
        &mut app,
        "1. e4 e5 2. Nf3 Nc6 3. Bc4 Nf6 4. d3 *",
        "Italian",
        None,
    );
    let matched = date_only_import(
        &mut app,
        "italian-play",
        "2024.10.06",
        "1. e4 e5 2. Nf3 Nc6 3. Bc4 Nf6 4. d3",
    );
    assert_eq!(matched["deviations"], 0, "{matched}");
    assert_eq!(matched["matches"], 4);
    let deviation = date_only_import(
        &mut app,
        "opponent-deviation",
        "2024.10.06",
        "1. e4 c6 2. d4",
    );
    assert_eq!(deviation["deviations"], 0);
    let report = date_only_import(
        &mut app,
        "italian-miss",
        "2024.10.06",
        "1. e4 e5 2. Nf3 Nc6 3. Bc4 Nf6 4. O-O",
    );
    assert_eq!(report["results"][0]["effect"], "again_applied");
    let repair = study(&mut app, json!({"command":"next"}));
    assert_eq!(repair["corrective_repair"], true);
    assert_eq!(repair["lesson"]["move_uci"], "d2d3");
    assert_eq!(repair["lesson"]["previous_move"], "g8f6");
    assert_eq!(
        play_activity(&mut app, &repair, "acknowledge", "deep-repair", "d2d3")["assessment"],
        Value::Null
    );
    let snapshot = app.snapshot().unwrap();
    assert!(snapshot.assessments.is_empty());
    assert_eq!(
        snapshot
            .learning
            .values()
            .filter(|state| state.introduced)
            .count(),
        1
    );
}

#[test]
fn a_saved_choice_selects_one_target_and_stays_resolved_after_reprocessing_and_restore() {
    let directory = tempfile::tempdir().unwrap();
    let today = Arc::new(AtomicI32::new(20_004));
    let db = directory.path().join("choices.sqlite");
    let limits = IntroductionLimits::default();
    let mut app = open(&db, today.clone(), limits, 0);
    preview_and_save(&mut app, "1. e4 *", "First", None);
    preview_and_save(&mut app, "1. d4 *", "Second", None);
    let report = date_only_import(&mut app, "unresolved", "2024.10.06", "1. c4 e5");
    let result = &report["results"][0];
    assert_eq!(result["effect"], "choice_required");
    let targets = result["choices"][0]["candidates"].as_array().unwrap();
    assert_eq!(targets.len(), 2);
    let request = |id: &str, target: &Value, ply: usize| {
        serde_json::from_value(json!({
        "command_id": id, "dataset_id":"opening-proof", "learner":"learner",
        "source":result["source"], "game_id":result["game_id"], "comparison_id":result["comparison_id"],
        "ply":ply, "target_id":target["target_id"]
    })).unwrap()
    };
    assert!(
        app.choose_game_correction(request("wrong-ply", &targets[0], 2))
            .is_err()
    );
    let choice = app
        .choose_game_correction(request("choose", &targets[0], 1))
        .unwrap();
    assert_eq!(
        serde_json::to_value(&choice).unwrap()["results"][0]["effect"],
        "again_applied"
    );
    assert_eq!(
        serde_json::to_vec(&choice).unwrap(),
        serde_json::to_vec(
            &app.choose_game_correction(request("choose", &targets[0], 1))
                .unwrap()
        )
        .unwrap()
    );
    assert!(
        app.choose_game_correction(request("change-choice", &targets[1], 1))
            .is_err()
    );
    let repair = study(&mut app, json!({"command":"next"}));
    assert_eq!(repair["target_id"], targets[0]["target_id"]);
    assert_eq!(repair_context(&app, &repair)["played_san"], "c4");
    let start = 20_002_u64 * 86_400_000;
    let reprocessed = app
        .reprocess_games(
            serde_json::from_value(json!({
                "command_id":"saved-window","dataset_id":"opening-proof","learner":"learner",
                "from_ms":start, "to_ms":start + 86_400_000
            }))
            .unwrap(),
        )
        .unwrap();
    assert_eq!(reprocessed.fetched, 1);
    assert!(reprocessed.results[0].summary.choices.is_empty());
    let store = SqliteStore::open(&db).unwrap();
    let dataset = app.snapshot().unwrap().dataset;
    let relevance = store.game_relevance(&dataset, "").unwrap();
    let selected = TargetId::from(targets[0]["target_id"].as_str().unwrap());
    let other = TargetId::from(targets[1]["target_id"].as_str().unwrap());
    assert_eq!(
        (relevance[&selected].exposure, relevance[&selected].misses),
        (1, 1)
    );
    assert_eq!(
        (relevance[&other].exposure, relevance[&other].misses),
        (1, 0)
    );
    let package = app.export(&tempo::JsonSnapshotCodec).unwrap();
    let mut restored = open(
        &directory.path().join("choices-restored.sqlite"),
        today,
        limits,
        0,
    );
    restored
        .restore("restored".into(), &package, &tempo::JsonSnapshotCodec)
        .unwrap();
    let duplicate = date_only_import(&mut restored, "same-game", "2024.10.06", "1. c4 e5");
    assert_eq!(duplicate["results"][0]["effect"], "already_addressed");
    assert!(duplicate["results"][0].get("choices").is_none());
}

#[test]
fn reset_accepts_same_day_date_only_games_and_keeps_earlier_games_historical() {
    let directory = tempfile::tempdir().unwrap();
    let today = Arc::new(AtomicI32::new(20_000));
    let mut app = open(
        &directory.path().join("reset-day.sqlite"),
        today,
        IntroductionLimits::default(),
        0,
    );
    let (_, saved) = preview_and_save(&mut app, "1. e4 *", "White", None);
    let id = &saved["repertoires"][0]["id"];
    let reset = content(
        &mut app,
        json!({"command":"reset_progress","dataset_id":"opening-proof",
        "repertoire_id":id,"command_id":"reset-day"}),
    );
    assert_eq!(reset["status"], "progress_reset");
    assert_eq!(
        date_only_import(&mut app, "earlier", "2024.10.03", "1. d4 d5")["results"][0]["effect"],
        "recorded_only"
    );
    assert_eq!(
        date_only_import(&mut app, "same-day", "2024.10.04", "1. d4 d5")["results"][0]["effect"],
        "again_applied"
    );
}

#[test]
fn completed_lichess_pgn_enriches_timing_and_replays_after_repertoire_edit() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("public-game.sqlite");
    let today = Arc::new(AtomicI32::new(20_647));
    let limits = IntroductionLimits::default();
    let mut app = open(&db, today.clone(), limits, 0);
    let (_, saved) = preview_and_save(&mut app, "1. e4 *", "Public game proof", None);
    let repertoire = saved["repertoires"][0]["id"].as_str().unwrap();
    let lesson = study(&mut app, json!({"command":"next"}));
    let mut teach = activity_command("acknowledge", &lesson, "teach-before-real-game");
    teach["answer"] = json!({"uci":"e2e4"});
    assert_eq!(study(&mut app, teach)["assessment"], Value::Null);
    today.store(20_648, Ordering::Relaxed);
    let ordinary = study(&mut app, json!({"command":"next"}));
    let mut answer = activity_command("answer", &ordinary, "test-before-real-game");
    answer["answer"] = json!({"uci":"e2e4"});
    assert_eq!(study(&mut app, answer)["assessment"], "good");
    today.store(20_650, Ordering::Relaxed);
    let ordinary = study(&mut app, json!({"command":"next"}));
    let mut answer = activity_command("answer", &ordinary, "second-test-before-real-game");
    answer["answer"] = json!({"uci":"e2e4"});
    assert_eq!(study(&mut app, answer)["assessment"], "good");
    today.store(20_652, Ordering::Relaxed);
    assert_eq!(app.study_progress(None).unwrap().unwrap().due, 0);

    // The provider archive supplies the occurrence bounds included in this upload.
    let command = json!({
        "repertoire_id": repertoire, "learner":"LeelaAlien",
        "command_id":"verified-public-game", "dataset_id":"opening-proof",
        "pgn":include_str!("../fixtures/lichess-LhL9TRJY.pgn"),
    });
    let mut initial = command.clone();
    initial["command_id"] = json!("game-without-end");
    let without_end = include_str!("../fixtures/lichess-LhL9TRJY.pgn")
        .lines()
        .filter(|line| !line.starts_with("[UTCEnd"))
        .collect::<Vec<_>>()
        .join("\n");
    initial["pgn"] = json!(&without_end);
    let original = app
        .import_game_pgn(serde_json::from_value(initial.clone()).unwrap())
        .unwrap();
    assert_eq!(original.results[0].summary.occurrence_end_ms, None);
    assert_eq!(app.study_progress(None).unwrap().unwrap().due, 0);

    let report = app
        .import_game_pgn(serde_json::from_value(command.clone()).unwrap())
        .unwrap();
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(report.results[0].summary.first_deviation_ply, Some(1));
    assert_eq!(
        serde_json::to_value(&report).unwrap()["results"][0]["effect"],
        "already_addressed"
    );
    let repair = study(&mut app, json!({"command":"next"}));
    assert_eq!(repair["corrective_repair"], true);
    assert_eq!(
        play_activity(&mut app, &repair, "acknowledge", "real-game-repair", "e2e4")["assessment"],
        Value::Null
    );
    assert_eq!(
        app.game_history().unwrap()["results"][0]["effect"],
        "again_applied"
    );

    let mut omission = initial.clone();
    omission["command_id"] = json!("new-command-without-end");
    let omission = app
        .import_game_pgn(serde_json::from_value(omission).unwrap())
        .unwrap();
    assert_eq!(
        serde_json::to_value(&omission).unwrap()["results"][0]["effect"],
        "already_addressed"
    );
    assert_eq!(
        omission.results[0].summary.occurrence_end_ms,
        report.results[0].summary.occurrence_end_ms
    );
    preview_and_save(&mut app, "1. h3 *", "Public game proof", Some(repertoire));
    drop(app);
    let mut app = open(&db, today, limits, 1);
    let replay = app
        .import_game_pgn(serde_json::from_value(command.clone()).unwrap())
        .unwrap();
    assert_eq!(
        serde_json::to_value(report).unwrap(),
        serde_json::to_value(replay).unwrap()
    );
    assert_eq!(
        app.game_history().unwrap()["results"][0]["effect"],
        "again_applied"
    );
    let mut reanalyze = command;
    reanalyze["command_id"] = json!("reanalyze-after-edit");
    let report = app
        .import_game_pgn(serde_json::from_value(reanalyze).unwrap())
        .unwrap();
    assert_eq!(
        (report.duplicates, report.matches, report.deviations),
        (1, 1, 0)
    );
    assert_eq!(app.snapshot().unwrap().assessments.len(), 2);
    let history = app.game_history().unwrap();
    let rows = history["results"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["game_id"], rows[1]["game_id"]);
    assert!(rows[0]["comparison_id"].is_string());
    assert_ne!(rows[0]["comparison_id"], rows[1]["comparison_id"]);
    let original_replay = app
        .import_game_pgn(serde_json::from_value(initial).unwrap())
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&original).unwrap(),
        serde_json::to_vec(&original_replay).unwrap()
    );
}

#[test]
fn pgn_batch_resolves_timing_in_either_order_and_rejects_conflicting_end_atomically() {
    let without_end = r#"[White "Learner"]
[Black "Other"]
[UTCDate "2024.10.05"]
[UTCTime "12:00:00"]

1. d4 d5 1-0"#;
    let with_end = without_end.replace(
        "[UTCTime \"12:00:00\"]",
        "[UTCTime \"12:00:00\"]\n[UTCEndDate \"2024.10.05\"]\n[UTCEndTime \"12:10:00\"]",
    );
    for reverse in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let db = directory.path().join("pgn-timing.sqlite");
        let today = Arc::new(AtomicI32::new(20_000));
        let mut app = open(&db, today.clone(), IntroductionLimits::default(), 0);
        let (_, saved) = preview_and_save(&mut app, "1. e4 *", "White", None);
        let repertoire = saved["repertoires"][0]["id"].as_str().unwrap();
        let lesson = study(&mut app, json!({"command":"next"}));
        let mut teach = activity_command("acknowledge", &lesson, "teach-before-pgn");
        teach["answer"] = json!({"uci":"e2e4"});
        assert_eq!(study(&mut app, teach)["assessment"], Value::Null);
        today.store(20_001, Ordering::Relaxed);
        let ordinary = study(&mut app, json!({"command":"next"}));
        let mut answer = activity_command("answer", &ordinary, "test-before-pgn");
        answer["answer"] = json!({"uci":"e2e4"});
        assert_eq!(study(&mut app, answer)["assessment"], "good");
        today.store(20_002, Ordering::Relaxed);

        let request = |command_id: &str, pgn: &str| {
            serde_json::from_value(json!({
                "repertoire_id": repertoire, "learner":"learner", "dataset_id":"opening-proof",
                "command_id":command_id, "pgn":pgn,
            }))
            .unwrap()
        };
        let pgn = if reverse {
            format!("{with_end}\n\n{without_end}")
        } else {
            format!("{without_end}\n\n{with_end}")
        };
        let report = app.import_game_pgn(request("batch", &pgn)).unwrap();
        assert_eq!(
            (report.new_games, report.duplicates, report.results.len()),
            (1, 1, 2)
        );
        let rows = serde_json::to_value(report).unwrap();
        let end = &rows["results"][0]["occurrence_end_ms"];
        assert!(end.is_number());
        assert_eq!(*end, rows["results"][1]["occurrence_end_ms"]);
        assert_eq!(
            rows["results"][0]["comparison_id"],
            rows["results"][1]["comparison_id"]
        );
        assert_eq!(rows["results"][0]["effect"], "again_applied");
        assert_eq!(rows["results"][1]["effect"], "already_addressed");

        let snapshot = app.snapshot().unwrap();
        let history = app.game_history().unwrap();
        let contradictory = with_end.replace("12:10:00", "12:11:00");
        let unrelated = with_end.replace("1. d4 d5", "1. c4 e5");
        let conflict = format!("{unrelated}\n\n{contradictory}");
        let error = app
            .import_game_pgn(request("conflict", &conflict))
            .err()
            .unwrap();
        assert_eq!(error.code, "store_conflict");
        assert_eq!(app.snapshot().unwrap(), snapshot);
        assert_eq!(app.game_history().unwrap(), history);

        // Omission keeps the known value, and the failed batch accepted no receipt.
        let omission = serde_json::to_value(
            app.import_game_pgn(request("conflict", without_end))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(omission["results"][0]["occurrence_end_ms"], *end);
        assert_eq!(omission["duplicates"], 1);
        assert_eq!(app.snapshot().unwrap(), snapshot);
        assert_eq!(app.game_history().unwrap(), history);

        let invalid = with_end.replace("12:10:00", "11:59:59");
        let invalid = app.import_game_pgn(request("reversed", &invalid)).unwrap();
        assert!(invalid.results.is_empty());
        assert_eq!(
            invalid.errors,
            ["Game 1: PGN has an invalid occurrence interval."]
        );

        // A separate raw game adds a failure once, then timing enriches only evidence.
        let later_identity = without_end.replace("1. d4 d5", "1. c4 e5");
        let undecided = app
            .import_game_pgn(request("other-unknown", &later_identity))
            .unwrap();
        assert_eq!(
            serde_json::to_value(undecided).unwrap()["results"][0]["effect"],
            "again_applied"
        );
        let after_test = app.snapshot().unwrap();
        let resolved = app
            .import_game_pgn(request("other-enriched", &unrelated))
            .unwrap();
        assert_eq!(
            serde_json::to_value(resolved).unwrap()["results"][0]["effect"],
            "already_addressed"
        );
        assert_eq!(app.snapshot().unwrap(), after_test);
    }
}

#[test]
fn portable_restore_keeps_the_latest_reset_boundary_across_restarts() {
    let directory = tempfile::tempdir().unwrap();
    let day = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits::default();
    let mut source = open(
        &directory.path().join("reset-source.sqlite"),
        day.clone(),
        limits,
        0,
    );
    let (_, saved) = preview_and_save(&mut source, "1. e4 *", "White", None);
    for (day_number, id) in [(20_000, "first-reset"), (20_002, "latest-reset")] {
        day.store(day_number, Ordering::Relaxed);
        let response = content(
            &mut source,
            json!({
                "command":"reset_progress", "dataset_id":"opening-proof",
                "repertoire_id":saved["repertoires"][0]["id"], "command_id":id
            }),
        );
        assert_eq!(response["status"], "progress_reset", "{response}");
    }
    let before = date_only_import(&mut source, "between", "2024.10.05", "1. d4 d5");
    assert_eq!(before["results"][0]["effect"], "recorded_only");

    let path = directory.path().join("reset-destination.sqlite");
    let data = source.export(&tempo::JsonSnapshotCodec).unwrap();
    let mut restored = open(&path, day.clone(), limits, 0);
    restored
        .restore("restored".into(), &data, &tempo::JsonSnapshotCodec)
        .unwrap();
    drop(restored);
    let mut restored = open(&path, day, limits, 1);
    let restored_data = restored.export(&tempo::JsonSnapshotCodec).unwrap();
    assert_eq!(restored_data, data);
    let retried = date_only_import(&mut restored, "between", "2024.10.05", "1. d4 d5");
    assert_eq!(retried["results"][0]["effect"], "recorded_only");
    assert_eq!(
        restored.snapshot().unwrap().learning,
        source.snapshot().unwrap().learning
    );
    let after = date_only_import(&mut restored, "qualifying", "2024.10.06", "1. d4 d5");
    assert_eq!(after["results"][0]["effect"], "again_applied", "{after}");
}

#[test]
fn portable_restore_keeps_a_choice_that_applied_no_new_again() {
    let directory = tempfile::tempdir().unwrap();
    let day = Arc::new(AtomicI32::new(20_004));
    let limits = IntroductionLimits::default();
    let mut source = open(
        &directory.path().join("choice-source.sqlite"),
        day.clone(),
        limits,
        0,
    );
    preview_and_save(&mut source, "1. e4 *", "First", None);
    let first = date_only_import(&mut source, "automatic", "2024.10.06", "1. c4 e5");
    assert_eq!(first["results"][0]["effect"], "again_applied");
    let corrected = source
        .snapshot()
        .unwrap()
        .learning
        .into_iter()
        .find(|(_, state)| state.correction.is_some())
        .unwrap()
        .0;

    preview_and_save(&mut source, "1. d4 *", "Second", None);
    let ambiguous = date_only_import(&mut source, "ambiguous", "2024.10.06", "1. c4 e5");
    let result = &ambiguous["results"][0];
    assert_eq!(result["effect"], "choice_required");
    let before = source.snapshot().unwrap();
    let accepted = source.choose_game_correction(serde_json::from_value(json!({
        "command_id":"choose-previously-corrected", "dataset_id":"opening-proof", "learner":"learner",
        "source":result["source"], "game_id":result["game_id"], "comparison_id":result["comparison_id"],
        "ply":1, "target_id":corrected
    })).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(accepted).unwrap()["results"][0]["effect"],
        "already_addressed"
    );
    assert_eq!(source.snapshot().unwrap().learning, before.learning);

    let data = source.export(&tempo::JsonSnapshotCodec).unwrap();
    let snapshot: Value = serde_json::from_str(&data).unwrap();
    assert_eq!(snapshot["game_failures"].as_array().unwrap().len(), 1);
    assert!(
        snapshot["game_failures"][0]
            .get("chosen_position")
            .is_none()
    );
    let path = directory.path().join("choice-destination.sqlite");
    let mut restored = open(&path, day.clone(), limits, 0);
    restored
        .restore("restored".into(), &data, &tempo::JsonSnapshotCodec)
        .unwrap();
    drop(restored);
    let mut restored = open(&path, day, limits, 1);
    assert_eq!(restored.export(&tempo::JsonSnapshotCodec).unwrap(), data);
    let duplicate = date_only_import(&mut restored, "reimported", "2024.10.06", "1. c4 e5");
    assert_eq!(
        duplicate["results"][0]["effect"], "already_addressed",
        "{duplicate}"
    );
    assert!(duplicate["results"][0].get("choices").is_none());
    assert_eq!(restored.snapshot().unwrap().learning, before.learning);
}

#[test]
fn portable_legacy_review_stays_actionable_and_is_consumed_by_a_covering_test() {
    let directory = tempfile::tempdir().unwrap();
    let day = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits::default();
    let mut source = open(
        &directory.path().join("review-source.sqlite"),
        day.clone(),
        limits,
        0,
    );
    preview_and_save(&mut source, "1. e4 *", "White", None);
    let lesson = study(&mut source, json!({"command":"next"}));
    let target = lesson["target_id"].as_str().unwrap();
    play_activity(&mut source, &lesson, "acknowledge", "teach-legacy", "e2e4");
    day.store(20_001, Ordering::Relaxed);
    let test = study(&mut source, json!({"command":"next"}));
    play_activity(&mut source, &test, "answer", "test-legacy", "e2e4");
    let data = source.export(&tempo::JsonSnapshotCodec).unwrap();
    let mut legacy: Value = serde_json::from_str(&data).unwrap();
    legacy["schema_version"] = json!(2);
    legacy["learning"][target]["due"] = json!(20_200);
    legacy["learning"][target]["game_review"] = json!({
        "answer_version":1, "occurred_by_ms":20_001_u64 * 86_400_000 + 1, "actionable_day":20_002
    });

    let path = directory.path().join("review-destination.sqlite");
    let mut restored = open(&path, day.clone(), limits, 0);
    restored
        .restore(
            "restored".into(),
            &legacy.to_string(),
            &tempo::JsonSnapshotCodec,
        )
        .unwrap();
    drop(restored);
    let mut restored = open(&path, day.clone(), limits, 1);
    restored.reconcile_legacy_game_requests().unwrap();
    assert_eq!(restored.study_progress(None).unwrap().unwrap().due, 0);
    assert_eq!(
        restored.snapshot().unwrap().learning[&TargetId::from(target)].scheduled_reviews,
        1
    );
    day.store(20_002, Ordering::Relaxed);
    let requested = study(&mut restored, json!({"command":"next"}));
    assert_eq!(requested["activity"], "test", "{requested}");
    assert_eq!(requested["target_id"], target);
    let accepted = play_activity(&mut restored, &requested, "answer", "covering-test", "e2e4");
    assert_eq!(accepted["assessment"], "good", "{accepted}");
    let after = restored.snapshot().unwrap();
    let state = &after.learning[&TargetId::from(target)];
    assert!(state.restored_game_obligation.is_none());
    assert!(state.correction.is_none());
    assert_eq!(state.scheduled_reviews, 2);
    let portable: Value =
        serde_json::from_str(&restored.export(&tempo::JsonSnapshotCodec).unwrap()).unwrap();
    assert!(portable.get("game_failures").is_none());
}

#[test]
fn portable_legacy_teaching_relevance_prioritizes_an_eligible_route() {
    let directory = tempfile::tempdir().unwrap();
    let day = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits {
        repertoire: 1,
        tactics: 0,
    };
    let mut source = open(
        &directory.path().join("teach-source.sqlite"),
        day.clone(),
        limits,
        0,
    );
    preview_and_save(&mut source, "1. e4 *", "First", None);
    preview_and_save(&mut source, "1. d4 *", "Second", None);
    let baseline = study(&mut source, json!({"command":"next"}));
    let baseline_id = TargetId::from(baseline["target_id"].as_str().unwrap());
    let relevant = source
        .snapshot()
        .unwrap()
        .learning
        .into_keys()
        .find(|target| target != &baseline_id)
        .unwrap();

    let mut legacy: Value =
        serde_json::from_str(&source.export(&tempo::JsonSnapshotCodec).unwrap()).unwrap();
    legacy["schema_version"] = json!(2);
    legacy["learning"][&relevant.0]["game_teaching_relevance"] = json!(true);
    let path = directory.path().join("teach-destination.sqlite");
    let mut restored = open(&path, day, limits, 0);
    restored
        .restore(
            "restored".into(),
            &legacy.to_string(),
            &tempo::JsonSnapshotCodec,
        )
        .unwrap();
    let lesson = study(&mut restored, json!({"command":"next"}));
    assert_eq!(lesson["activity"], "teach", "{lesson}");
    assert_eq!(lesson["target_id"], relevant.0);
    let uci = lesson["lesson"]["move_uci"].as_str().unwrap();
    let accepted = play_activity(&mut restored, &lesson, "acknowledge", "relevant-teach", uci);
    assert_eq!(accepted["status"], "complete", "{accepted}");
    let state = &restored.snapshot().unwrap().learning[&relevant];
    assert!(state.restored_game_obligation.is_none());
    assert_eq!(state.scheduled_reviews, 0);
    assert_eq!(
        study(&mut restored, json!({"command":"study_status"}))["repertoire_remaining"],
        0
    );
}

#[test]
fn portable_restore_rejects_corrective_recall_on_the_game_assessment_day() {
    let directory = tempfile::tempdir().unwrap();
    let day = Arc::new(AtomicI32::new(20_004));
    let limits = IntroductionLimits::default();
    let mut source = open(
        &directory.path().join("timing-source.sqlite"),
        day.clone(),
        limits,
        0,
    );
    preview_and_save(&mut source, "1. e4 *", "White", None);
    let game = date_only_import(&mut source, "miss", "2024.10.06", "1. c4 e5");
    assert_eq!(game["results"][0]["effect"], "again_applied");
    let package = source.export(&tempo::JsonSnapshotCodec).unwrap();
    let mut invalid: Value = serde_json::from_str(&package).unwrap();
    let state = invalid["learning"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap();
    state["correction"]["awaiting_introduction"] = json!(false);
    state["correction"]["next_recall_day"] = state["last_review_at"]["day"].clone();

    let mut restored = open(
        &directory.path().join("timing-destination.sqlite"),
        day,
        limits,
        0,
    );
    let before = restored.snapshot().unwrap();
    let rejected = restored.restore(
        "restored".into(),
        &invalid.to_string(),
        &tempo::JsonSnapshotCodec,
    );
    assert_eq!(rejected.unwrap_err().code, "invalid_learning");
    assert_eq!(restored.snapshot().unwrap(), before);
    restored
        .restore("restored".into(), &package, &tempo::JsonSnapshotCodec)
        .unwrap();
}

#[test]
fn importing_two_distinct_game_misses_commits_the_final_learning_transition() {
    let directory = tempfile::tempdir().unwrap();
    let day = Arc::new(AtomicI32::new(20_004));
    let db = directory.path().join("multiple-games.sqlite");
    let mut app = open(&db, day, IntroductionLimits::default(), 0);
    preview_and_save(&mut app, "1. e4 *", "White", None);
    let request = json!({
        "command_id":"two-games", "dataset_id":"opening-proof", "learner":"learner",
        "pgn": "[White \"Learner\"]\n[Black \"Other\"]\n[Date \"2024.10.06\"]\n1. c4 e5 1-0\n\n[White \"Learner\"]\n[Black \"Other\"]\n[Date \"2024.10.07\"]\n1. d4 d5 1-0"
    });
    let result = app
        .import_game_pgn(serde_json::from_value(request.clone()).unwrap())
        .unwrap();
    let report = serde_json::to_value(result).unwrap();
    assert_eq!(report["results"][0]["effect"], "again_applied", "{report}");
    assert_eq!(report["results"][1]["effect"], "again_applied", "{report}");
    let saved = app.snapshot().unwrap();
    let state = saved.learning.values().next().unwrap();
    assert_eq!(state.scheduled_reviews, 2);
    assert_eq!(state.miss_revision.0, 2);
    assert_eq!(
        SqliteStore::open(&db)
            .unwrap()
            .game_failures(&saved.dataset)
            .unwrap()
            .len(),
        2
    );
    let replay = app
        .import_game_pgn(serde_json::from_value(request).unwrap())
        .unwrap();
    assert_eq!(serde_json::to_value(replay).unwrap(), report);
    assert_eq!(app.snapshot().unwrap(), saved);
}

const RELEVANCE_BOOK: &str = "1. e4 e5 (1... c5 2. Nf3 d6 3. d4) 2. Nf3 Nc6 3. Bb5 *";

fn observed_move(
    app: &mut Tempo,
    command_id: &str,
    opponent: &str,
    fen: &str,
    san: &str,
) -> tempo::game_evidence::GameReport {
    let pgn = format!(
        "[White \"learner\"]\n[Black \"{opponent}\"]\n[Date \"2024.10.02\"]\n[SetUp \"1\"]\n[FEN \"{fen}\"]\n\n{san} 1-0"
    );
    app.import_game_pgn(
        serde_json::from_value(json!({
            "command_id":command_id, "dataset_id":"opening-proof", "learner":"learner", "pgn":pgn,
        }))
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn correctly_played_positions_change_due_review_order_without_admitting_future_work() {
    let directory = tempfile::tempdir().unwrap();
    let day = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits::default();
    let first = directory.path().join("no-evidence.sqlite");
    let mut baseline = open(&first, day.clone(), limits, 0);
    preview_and_save(&mut baseline, RELEVANCE_BOOK, "One book", None);
    for index in 0..5 {
        let card = study(&mut baseline, json!({"command":"next"}));
        assert_eq!(card["activity"], "teach");
        assert_eq!(
            play_activity(
                &mut baseline,
                &card,
                "acknowledge",
                &format!("introduce-{index}"),
                card["lesson"]["move_uci"].as_str().unwrap()
            )["status"],
            "complete"
        );
    }
    let before = baseline.snapshot().unwrap();
    drop(baseline);
    let ranked = directory.path().join("with-evidence.sqlite");
    std::fs::copy(&first, &ranked).unwrap();
    day.store(20_001, Ordering::Relaxed);
    let mut baseline = open(&first, day.clone(), limits, 1);
    let baseline_progress = baseline.study_progress(None).unwrap().unwrap();
    let initially = study(&mut baseline, json!({"command":"next"}));
    let ranked_targets: Vec<_> = before
        .catalog
        .items
        .iter()
        .filter(|(target, _)| initially["target_id"] != target.0)
        .take(2)
        .collect();
    let (now_target, item) = ranked_targets[0];
    let (future_target, later_item) = ranked_targets[1];

    let mut app = open(&ranked, day.clone(), limits, 1);
    let mut store = SqliteStore::open(&ranked).unwrap();
    let mut future = before.learning[future_target].clone();
    future.due = Some(StudyDay(20_002));
    store
        .commit(tempo_store::Commit {
            expected_dataset: before.dataset.clone(),
            expected_revision: before.revision,
            writes: vec![tempo_store::Write::Learning {
                target: future_target.clone(),
                state: future,
            }],
        })
        .unwrap();
    let decision = &item.definition.decisions[0];
    let first = observed_move(
        &mut app,
        "one-correct",
        "first",
        &decision.fen,
        &item.lesson.as_ref().unwrap().san,
    );
    let duplicate = observed_move(
        &mut app,
        "same-correct",
        "first",
        &decision.fen,
        &item.lesson.as_ref().unwrap().san,
    );
    assert_eq!(
        (first.matches, first.new_games, duplicate.duplicates),
        (1, 1, 1)
    );
    let again = observed_move(
        &mut app,
        "one-correct",
        "first",
        &decision.fen,
        &item.lesson.as_ref().unwrap().san,
    );
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&again).unwrap()
    );
    for (command, opponent) in [("two-correct", "second"), ("three-correct", "third")] {
        observed_move(
            &mut app,
            command,
            opponent,
            &later_item.definition.decisions[0].fen,
            &later_item.lesson.as_ref().unwrap().san,
        );
    }
    let evidence = store.game_relevance(&before.dataset, "").unwrap();
    assert_eq!(
        (
            evidence[now_target].exposure,
            evidence[now_target].misses,
            evidence[now_target].window_games
        ),
        (1, 0, 3)
    );
    assert_eq!(evidence[future_target].exposure, 2);
    assert!(app.snapshot().unwrap().assessments.is_empty());
    let with_evidence = app.study_progress(None).unwrap().unwrap();
    assert_eq!(with_evidence.due + 1, baseline_progress.due);
    drop(app);
    let no_evidence = directory.path().join("removed-evidence.sqlite");
    std::fs::copy(&ranked, &no_evidence).unwrap();
    let mut app = open(&ranked, day.clone(), limits, 2);
    let selected = study(&mut app, json!({"command":"next"}));
    assert_eq!(selected["activity"], "test");
    assert_eq!(selected["target_id"], now_target.0);
    let id = selected["attempt_id"].as_str().unwrap().into();
    assert_eq!(
        app.study_relevance_reason(&before.dataset, &id).unwrap()["reason"],
        "Reached in 1 of your last 3 games."
    );
    drop(app);
    let mut app = open(&ranked, day.clone(), limits, 3);
    assert_eq!(
        study(&mut app, json!({"command":"next"}))["attempt_id"],
        selected["attempt_id"]
    );
    assert_eq!(
        app.study_relevance_reason(&before.dataset, &id).unwrap()["reason"],
        "Reached in 1 of your last 3 games."
    );

    let mut filtered = SqliteStore::open(&no_evidence).unwrap();
    filtered
        .save_game_backfill_state(
            &before.dataset,
            "config",
            "{}",
            Some(&[("lichess".into(), "other".into())]),
        )
        .unwrap();
    assert!(
        filtered
            .game_relevance(&before.dataset, "")
            .unwrap()
            .is_empty()
    );
    let mut filtered = open(&no_evidence, day, limits, 2);
    assert_eq!(
        filtered.study_progress(None).unwrap().unwrap().due,
        with_evidence.due
    );
    assert_eq!(
        study(&mut filtered, json!({"command":"next"}))["target_id"],
        initially["target_id"]
    );
}

#[test]
fn deeper_game_positions_advance_their_own_eligible_frontier_in_either_branch() {
    let directory = tempfile::tempdir().unwrap();
    let day = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits::default();
    let baseline = directory.path().join("branch-baseline.sqlite");
    let mut app = open(&baseline, day.clone(), limits, 0);
    preview_and_save(&mut app, RELEVANCE_BOOK, "Both branches", None);
    let root = study(&mut app, json!({"command":"next"}));
    assert_eq!(root["lesson"]["move_uci"], "e2e4");
    assert_eq!(
        play_activity(&mut app, &root, "acknowledge", "root", "e2e4")["status"],
        "complete"
    );
    let snapshot = app.snapshot().unwrap();
    drop(app);
    let mut selected = Vec::new();
    for (index, route) in snapshot.catalog.lines.iter().enumerate() {
        let db = directory.path().join(format!("branch-{index}.sqlite"));
        std::fs::copy(&baseline, &db).unwrap();
        let mut app = open(&db, day.clone(), limits, 1);
        let target = &snapshot.catalog.items[&route.targets[2]];
        let report = observed_move(
            &mut app,
            "deeper",
            "opponent",
            &target.definition.decisions[0].fen,
            &target.lesson.as_ref().unwrap().san,
        );
        assert_eq!(report.matches, 1);
        let found = study(&mut app, json!({"command":"next"}));
        assert_eq!(found["activity"], "teach");
        assert_eq!(found["target_id"], route.targets[1].0);
        let reason = app
            .study_relevance_reason(
                &snapshot.dataset,
                &found["attempt_id"].as_str().unwrap().into(),
            )
            .unwrap();
        assert_eq!(
            reason["reason"],
            "A later position was reached in 1 of your last 1 game."
        );
        assert!(!app.snapshot().unwrap().learning[&route.targets[2]].introduced);
        selected.push(found["target_id"].clone());
    }
    assert_eq!(selected.len(), 2);
    assert_ne!(selected[0], selected[1]);
}

#[test]
fn marked_line_preview_and_exact_save_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("marked.sqlite");
    let today = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits::default();
    let mut app = open(&db, today.clone(), limits, 0);
    let draft = json!({
        "command":"marked_preview", "dataset_id":"opening-proof", "name":"Board book",
        "learner_color":"white", "repertoire_id":null, "base_revision":null,
        "anchor_fen":tempo_chess::START_FEN,
        "lines":[["e2e4","c7c5","g1f3"],["e2e4","e7e5","g1f3"]]
    });
    let preview = content(&mut app, draft.clone());
    assert_eq!(preview["status"], "preview", "{preview}");
    assert_eq!(preview["routes"], 2);
    let saved = content(&mut app, preview["save_command"].clone());
    assert_eq!(saved["status"], "repertoires", "{saved}");
    drop(app);
    let mut app = open(&db, today, limits, 1);
    assert_eq!(content(&mut app, preview["save_command"].clone()), saved);
    for index in 0..3 {
        let lesson = study(&mut app, json!({"command":"next"}));
        let mut ack = activity_command("acknowledge", &lesson, &format!("marked-lesson-{index}"));
        ack["answer"] = json!({"uci":lesson["lesson"]["move_uci"]});
        assert_eq!(study(&mut app, ack)["status"], "complete");
    }
    let before = app.snapshot().unwrap();
    let document = content(
        &mut app,
        json!({"command":"read_document","dataset_id":"opening-proof","repertoire_id":preview["repertoire_id"]}),
    );
    let mut replace = draft;
    replace["repertoire_id"] = preview["repertoire_id"].clone();
    replace["base_revision"] = document["revision"].clone();
    replace["lines"] = json!([["d2d4"]]);
    let next = content(&mut app, replace.clone());
    assert_eq!(next["status"], "preview", "{next}");
    assert_eq!(next["replacements"][0]["old_move"], "e4");
    assert_eq!(next["affected_learning"], 3);
    assert_eq!(next["removed"], 2);
    assert_eq!(next["changed"], 1);
    assert_eq!(next["retired_routes"].as_array().unwrap().len(), 2);
    assert_eq!(
        content(&mut app, next["save_command"].clone())["status"],
        "repertoires"
    );
    let after = app.snapshot().unwrap();
    for (id, previous) in &before.catalog.items {
        let item = &after.catalog.items[id];
        if item.active() {
            assert_eq!(item.version.0, previous.version.0 + 1);
            assert_eq!(
                after.learning[id],
                DailyLearning::changed_answer(&before.learning[id])
            );
        } else {
            assert_eq!(item.version, previous.version);
            assert_eq!(after.learning[id], before.learning[id]);
        }
    }
    assert_eq!(after.assessments, before.assessments);
    assert_eq!(content(&mut app, replace)["code"], "stale_draft");
}

fn issued_analysis_line(app: &Tempo, view: &Value) -> Result<Value, tempo::AppError> {
    use tempo_types::{AttemptId, ContentVersion, DatasetId, Revision};
    app.study_analysis_line(
        &DatasetId::from(view["dataset_id"].as_str().unwrap()),
        &AttemptId::from(view["attempt_id"].as_str().unwrap()),
        Revision(view["revision"].as_u64().unwrap()),
        ContentVersion(view["content_version"].as_u64().unwrap()),
    )
    .map(|line| serde_json::to_value(line).unwrap())
}

#[test]
fn conceding_a_scheduled_review_recovers_its_issued_position_once() {
    use tempo_types::{AttemptId, ContentVersion, DatasetId, Revision};

    fn analysis(app: &Tempo, view: &Value) -> Result<Value, tempo::AppError> {
        app.study_analysis_context(
            &DatasetId::from(view["dataset_id"].as_str().unwrap()),
            &AttemptId::from(view["attempt_id"].as_str().unwrap()),
            Revision(view["revision"].as_u64().unwrap()),
            ContentVersion(view["content_version"].as_u64().unwrap()),
        )
    }

    for already_missed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("concession.sqlite");
        let today = Arc::new(AtomicI32::new(20_000));
        let mut app = open(&db, today.clone(), IntroductionLimits::default(), 0);
        let (_, saved) = preview_and_save(&mut app, "1. e4 *", "White", None);
        let book = saved["repertoires"][0]["id"].as_str().unwrap();
        let teach = study(&mut app, json!({"command":"next"}));
        let before = app.snapshot().unwrap();
        assert_eq!(analysis(&app, &teach).unwrap()["repertoireId"], book);
        assert_eq!(
            study(&mut app, activity_command("concede", &teach, "invalid"))["code"],
            "wrong_activity"
        );
        assert_eq!(app.snapshot().unwrap(), before);
        let mut ack = activity_command("acknowledge", &teach, "teach");
        ack["answer"] = json!({"uci":"e2e4"});
        assert_eq!(study(&mut app, ack)["status"], "complete");

        today.store(20_001, Ordering::Relaxed);
        let mut view = study(&mut app, json!({"command":"next"}));
        assert_eq!(view["activity"], "test");
        if already_missed {
            let mut wrong = activity_command("answer", &view, "a-miss");
            wrong["answer"] = json!({"uci":"e2e3"});
            view = study(&mut app, wrong);
        }
        assert_eq!(
            analysis(&app, &view).unwrap_err().code,
            "analysis_requires_concession"
        );
        let before = app.snapshot().unwrap();
        let concede = activity_command("concede", &view, "confirmed-concession");
        let closed = study(&mut app, concede.clone());
        assert_eq!(closed["status"], "abandoned", "{closed}");
        assert_eq!(closed["assessment"], "again");
        let after = app.snapshot().unwrap();
        assert_eq!(after.active_attempt, None);
        assert_eq!(after.assessments.len(), 1);
        assert_eq!(after.receipts.len(), before.receipts.len() + 1);
        if already_missed {
            assert_eq!(before.learning, after.learning);
        }
        let origin = analysis(&app, &closed).unwrap();
        assert_eq!(origin["fen"], view["display_fen"]);
        assert_eq!(origin["prescribedUci"], "e2e4");
        assert!(origin.get("analysisLine").is_none());
        let exported = issued_analysis_line(&app, &closed).unwrap();
        assert_eq!(exported["scope"], "full_line");
        assert_eq!(exported["line"]["moves"][0]["notation"], "1. e4");
        assert_eq!(
            origin["playedUci"],
            if already_missed {
                json!("e2e3")
            } else {
                Value::Null
            }
        );
        drop(app);

        let mut app = open(&db, today.clone(), IntroductionLimits::default(), 1);
        preview_and_save(&mut app, "1. d4 *", "White", Some(book));
        let after_edit = app.snapshot().unwrap();
        assert_eq!(study(&mut app, concede.clone()), closed);
        let edited = issued_analysis_line(&app, &closed).unwrap();
        assert_eq!(edited["scope"], "issued_move");
        assert_eq!(edited["line"]["moves"][0]["notation"], "1. e4");
        assert_eq!(analysis(&app, &closed).unwrap(), origin);
        assert_eq!(app.snapshot().unwrap(), after_edit);
        let mut duplicate = concede;
        duplicate["submission_id"] = "different-identity".into();
        assert_eq!(study(&mut app, duplicate)["status"], "error");
        assert_eq!(app.snapshot().unwrap(), after_edit);
    }
}

#[test]
fn study_analysis_exports_all_puzzle_plies_after_concession() {
    use tempo_types::{AttemptId, ContentVersion, DatasetId, Revision};

    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("tactic-analysis.sqlite");
    let today = Arc::new(AtomicI32::new(20_000));
    let mut app = open(&db, today, IntroductionLimits::default(), 0);
    let black = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1";
    let pgn = snippet("Black puzzle", black, "1... c5 2. Nf3 d6 3. d4");
    transfer_tactics(&mut app, &pgn, &[0], "fixture");
    let view = study(&mut app, json!({"command":"next"}));
    assert_eq!(view["activity"], "test");
    assert!(view.get("analysisLine").is_none());
    let context = |app: &Tempo, view: &Value| {
        app.study_analysis_context(
            &DatasetId::from(view["dataset_id"].as_str().unwrap()),
            &AttemptId::from(view["attempt_id"].as_str().unwrap()),
            Revision(view["revision"].as_u64().unwrap()),
            ContentVersion(view["content_version"].as_u64().unwrap()),
        )
    };
    assert_eq!(
        context(&app, &view).unwrap_err().code,
        "analysis_requires_concession"
    );
    assert_eq!(
        issued_analysis_line(&app, &view).unwrap_err().code,
        "analysis_requires_concession"
    );
    let closed = study(
        &mut app,
        activity_command("concede", &view, "puzzle-analysis"),
    );
    assert_eq!(closed["assessment"], "again");
    let before = app.snapshot().unwrap();
    let origin = context(&app, &closed).unwrap();
    assert_eq!(origin["color"], "black");
    assert!(origin.get("analysisLine").is_none());
    let exported = issued_analysis_line(&app, &closed).unwrap();
    let line = &exported["line"];
    assert_eq!(exported["scope"], "full_line");
    assert!(line["start_fen"].as_str().unwrap().contains(" b "));
    let moves: Vec<_> = line["moves"]
        .as_array()
        .unwrap()
        .iter()
        .map(|ply| ply["notation"].as_str().unwrap())
        .collect();
    assert_eq!(moves, ["1... c5", "2. Nf3", "2... d6", "3. d4"]);
    assert_eq!(app.snapshot().unwrap(), before);
}

#[test]
fn marked_preview_returns_a_signed_save_for_a_large_document() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = open(
        &directory.path().join("size.sqlite"),
        Arc::new(AtomicI32::new(20_000)),
        IntroductionLimits::default(),
        0,
    );
    let source = format!("[Site \"{}\"]\n1.e4 *", "a".repeat(2 * 1024 * 1024));
    let preview = content(&mut app, book_request("preview", &source, "White", None));
    assert_eq!(preview["status"], "preview");
    let mut save = book_request("save", &source, "White", None);
    save["token"] = preview["token"].clone();
    let saved = content(&mut app, save);
    let id = &saved["repertoires"][0]["id"];
    let document = content(
        &mut app,
        json!({"command":"read_document","dataset_id":"opening-proof","repertoire_id":id}),
    );
    let revision = app.snapshot().unwrap().revision;
    let extended = content(
        &mut app,
        json!({"command":"marked_preview","dataset_id":"opening-proof","repertoire_id":id,
        "name":"White","learner_color":"white","anchor_fen":tempo_chess::START_FEN,
        "base_revision":document["revision"],"lines":[["e2e4","e7e5","g1f3"]]}),
    );
    assert_eq!(extended["status"], "preview");
    assert_eq!(app.snapshot().unwrap().revision, revision);
    let save = extended["save_command"].clone();
    assert!(serde_json::to_vec(&save).unwrap().len() > 2 * 1024 * 1024);
    assert_eq!(content(&mut app, save)["status"], "repertoires");
    assert_eq!(app.snapshot().unwrap().revision.0, revision.0 + 1);
}

#[test]
fn a_saved_opening_uses_the_issued_definition_until_history_is_reset() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("saved-review.sqlite");
    let today = Arc::new(AtomicI32::new(20_000));
    let mut app = open(&db, today, IntroductionLimits::default(), 0);
    let (_, book) = preview_and_save(&mut app, "1. e4 *", "Before edit", None);
    let book_id = book["repertoires"][0]["id"].as_str().unwrap();
    let lesson = study(&mut app, json!({"command": "next"}));
    assert_eq!(lesson["activity"], "teach");
    let before = app.snapshot().unwrap();
    let save = json!({
        "dataset_id": lesson["dataset_id"],
        "attempt_id": lesson["attempt_id"],
        "content_version": lesson["content_version"],
        "position": {"kind": "decision", "index": 0},
        "submission_id": "saved-opening",
    });
    let saved = app
        .save_study_review(serde_json::from_value(save.clone()).unwrap())
        .unwrap();
    assert_eq!(app.snapshot().unwrap(), before);
    let mut acknowledgement = activity_command("acknowledge", &lesson, "teach-opening");
    acknowledgement["answer"] = json!({"uci": "e2e4"});
    assert_eq!(study(&mut app, acknowledgement)["status"], "complete");
    let renamed = content(
        &mut app,
        json!({
            "command": "rename", "dataset_id": lesson["dataset_id"],
            "repertoire_id": book_id, "name": "After edit",
        }),
    );
    assert_eq!(renamed["status"], "repertoires", "{renamed}");
    preview_and_save(&mut app, "1. d4 *", "After edit", Some(book_id));
    let open_mark = json!({
        "command": "open", "dataset_id": lesson["dataset_id"], "id": saved["id"],
    });
    let context = app
        .saved_study_reviews(serde_json::from_value(open_mark.clone()).unwrap())
        .unwrap();
    assert_eq!(context["origin"]["fen"], lesson["prompt"]["fen"]);
    assert_eq!(context["origin"]["name"], "Before edit");
    assert_eq!(context["origin"]["prescribedUci"], "e2e4");
    let reset = content(
        &mut app,
        json!({
            "command": "reset_progress", "dataset_id": lesson["dataset_id"],
            "repertoire_id": book_id, "command_id": "reset-after-mark",
        }),
    );
    assert_eq!(reset["status"], "progress_reset", "{reset}");
    let gone = app
        .saved_study_reviews(serde_json::from_value(open_mark).unwrap())
        .unwrap_err();
    assert_eq!(gone.code, "unknown_review");
    assert_eq!(
        app.save_study_review(serde_json::from_value(save).unwrap())
            .unwrap(),
        saved
    );
    let page = app
        .saved_study_reviews(
            serde_json::from_value(json!({
                "command": "list", "dataset_id": lesson["dataset_id"],
            }))
            .unwrap(),
        )
        .unwrap();
    assert_eq!(page["rows"], json!([]));
}

fn activity_change(repertoire: &str, command_id: &str, activity: &str) -> Value {
    json!({
        "command":"set_activity", "dataset_id":"opening-proof",
        "repertoire_id":repertoire, "command_id":command_id, "activity":activity,
    })
}

#[test]
fn repertoire_suspension_fences_study_and_preserves_learning_and_replay() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("suspend-study.sqlite");
    let day = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits {
        repertoire: 1,
        ..IntroductionLimits::default()
    };
    let mut app = open(&db, day.clone(), limits, 0);
    let pgn = "1. d4 d5 2. Bf4 Nf6 3. e3 *";
    let (_, list) = preview_and_save(&mut app, pgn, "London", None);
    let id = list["repertoires"][0]["id"].as_str().unwrap();
    let lesson = study(&mut app, json!({"command":"next"}));
    let mut ack = activity_command("acknowledge", &lesson, "teach-london");
    ack["answer"] = json!({"uci":"d2d4"});
    let taught = study(&mut app, ack.clone());
    assert_eq!(taught["status"], "complete", "{taught}");

    day.store(20_001, Ordering::Relaxed);
    let test = study(&mut app, json!({"command":"next"}));
    assert_eq!(test["activity"], "test", "{test}");
    let mut answer = activity_command("answer", &test, "review-london");
    answer["answer"] = json!({"uci":"d2d4"});
    let passed = study(&mut app, answer.clone());
    assert_eq!(passed["assessment"], "good", "{passed}");
    let lesson = study(&mut app, json!({"command":"next"}));
    assert_eq!(lesson["activity"], "teach", "{lesson}");
    let mut late_ack = activity_command("acknowledge", &lesson, "unaccepted-london");
    late_ack["answer"] = json!({"uci":"c1f4"});

    let before = app.snapshot().unwrap();
    let suspend = activity_change(id, "suspend-london", "suspended");
    let applied = content(&mut app, suspend.clone());
    assert_eq!(applied["status"], "activity_set", "{applied}");
    let suspended = app.snapshot().unwrap();
    assert_eq!(suspended.active_attempt, None);
    assert_eq!(
        suspended.attempts[before.active_attempt.as_ref().unwrap()]
            .attempt
            .status,
        tempo_study::AttemptStatus::Abandoned
    );
    assert_eq!(suspended.assessments, before.assessments);
    assert_eq!(suspended.learning, before.learning);
    assert_eq!(suspended.receipts, before.receipts);
    assert_eq!(suspended.catalog, before.catalog);

    drop(app);
    day.store(20_100, Ordering::Relaxed);
    let mut app = open(&db, day, limits, 1);
    assert_eq!(study(&mut app, late_ack)["code"], "suspended_repertoire");
    assert_eq!(study(&mut app, ack), taught);
    assert_eq!(study(&mut app, answer), passed);
    let empty = study(&mut app, json!({"command":"next"}));
    assert_eq!(empty["done"], true, "{empty}");
    assert_eq!(empty["outstanding"], 0);

    let renamed = content(
        &mut app,
        json!({
            "command":"rename", "dataset_id":"opening-proof", "repertoire_id":id, "name":"White d4",
        }),
    );
    assert_eq!(renamed["repertoires"][0]["activity"], "suspended");
    let (_, replaced) = preview_and_save(
        &mut app,
        "1. d4 d5 2. Bf4 Nf6 3. e3 e6 4. Nf3 *",
        "White d4",
        Some(id),
    );
    assert_eq!(replaced["repertoires"][0]["activity"], "suspended");
    preview_and_save(&mut app, SHORT, "Italian", None);
    let italian = study(&mut app, json!({"command":"next"}));
    assert_eq!(italian["exercise_name"], "Italian", "{italian}");

    let resume = activity_change(id, "resume-london", "active");
    assert_eq!(content(&mut app, resume)["activity"], "active");
    assert_eq!(content(&mut app, suspend), applied);
    assert_eq!(study(&mut app, json!({"command":"next"})), italian);
    let leave = activity_command("abandon", &italian, "leave-italian");
    assert_eq!(study(&mut app, leave)["status"], "abandoned");
    let test = study(&mut app, json!({"command":"next"}));
    assert_eq!(test["exercise_name"], "White d4", "{test}");
    assert_eq!(test["activity"], "test");
    assert_eq!(app.snapshot().unwrap().assessments, before.assessments);
}

#[test]
fn repertoire_suspension_excludes_game_checks_and_retains_prior_gap_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("suspend-games.sqlite");
    let day = Arc::new(AtomicI32::new(20_000));
    let mut app = open(&db, day, IntroductionLimits::default(), 0);
    let (_, list) = preview_and_save(&mut app, "1. d4 d5 2. Bf4 *", "London", None);
    let london = list["repertoires"][0]["id"].as_str().unwrap();
    let moves = "1. d4 Nf6 2. Bf4";
    let checked = date_only_import(&mut app, "before-suspension", "2024.10.01", moves);
    assert_eq!(
        checked["results"][0]["repertoires"][0]["repertoire_id"], london,
        "{checked}"
    );
    assert_eq!(
        app.opponent_gaps(0).unwrap()["gaps"][0]["repertoire_id"],
        london
    );
    let before = app.snapshot().unwrap();

    let suspended = content(
        &mut app,
        activity_change(london, "suspend-games", "suspended"),
    );
    assert_eq!(suspended["status"], "activity_set");
    assert_eq!(app.opponent_gaps(0).unwrap()["gaps"], json!([]));
    let raw = date_only_import(
        &mut app,
        "suspended-new-game",
        "2024.10.02",
        "1. d4 d5 2. Nf3",
    );
    assert_eq!(raw["new_games"], 1, "{raw}");
    assert!(raw["results"][0]["repertoires"].is_null(), "{raw}");
    assert_eq!(raw["results"][0]["coverage_gaps"], 0);
    assert_eq!(raw["results"][0]["matches"], 0);
    assert_eq!(raw["results"][0]["deviations"], 0);

    let review = json!({
        "dataset_id":"opening-proof","source":raw["results"][0]["source"],
        "game_id":raw["results"][0]["game_id"],"learner":"learner","repertoire_id":london,
    });
    assert_eq!(
        app.game_review(serde_json::from_value(review.clone()).unwrap())
            .unwrap_err()
            .code,
        "suspended_repertoire"
    );
    let mut timeline = review;
    timeline["repertoire_id"] = Value::Null;
    assert!(
        app.game_review(serde_json::from_value(timeline).unwrap())
            .is_ok()
    );
    date_only_import(&mut app, "suspended-old-game", "2024.10.01", moves);
    assert_eq!(app.snapshot().unwrap().learning, before.learning);
    assert_eq!(app.snapshot().unwrap().assessments, before.assessments);

    let (_, list) = preview_and_save(&mut app, SHORT, "Italian", None);
    let italian = list["repertoires"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "Italian")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let store = SqliteStore::open(&db).unwrap();
    let books = store
        .projection_books(
            &"opening-proof".into(),
            tempo_store::ProjectionScope::AllVerified,
        )
        .unwrap();
    assert_eq!(
        books
            .iter()
            .map(|book| book.repertoire_id.as_str())
            .collect::<Vec<_>>(),
        [italian]
    );
    let current = date_only_import(&mut app, "italian-only", "2024.10.03", "1. d4 d5 2. Bf4");
    assert_eq!(
        current["results"][0]["repertoires"][0]["repertoire_id"],
        italian
    );
    assert_eq!(
        current["results"][0]["repertoires"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(current["results"][0]["deviations"], 1, "{current}");
    assert_eq!(current["results"][0]["matches"], 0);

    assert_eq!(
        content(&mut app, activity_change(london, "resume-games", "active"))["activity"],
        "active"
    );
    assert_eq!(
        app.opponent_gaps(0).unwrap()["gaps"][0]["repertoire_id"],
        london
    );
}

#[test]
fn repertoire_suspension_leaves_another_books_signed_edit_valid() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("suspend-open-draft.sqlite");
    let mut app = open(
        &db,
        Arc::new(AtomicI32::new(20_000)),
        IntroductionLimits::default(),
        0,
    );
    let (_, created) = preview_and_save(&mut app, SHORT, "Italian", None);
    let italian = created["repertoires"][0]["id"].as_str().unwrap();
    let (_, books) = preview_and_save(&mut app, "1. d4 d5 2. Bf4 *", "London", None);
    let london = books["repertoires"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "London")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let document = content(
        &mut app,
        json!({
            "command":"read_document", "dataset_id":"opening-proof", "repertoire_id":italian,
        }),
    );
    let mut input = book_request(
        "edit_preview",
        "1. e4 e5 2. Bc4 Nc6 3. Nf3 *",
        "Italian",
        Some(italian),
    );
    input["base_revision"] = document["revision"].clone();
    let before = content(&mut app, input.clone());
    assert_eq!(before["status"], "preview", "{before}");

    assert_eq!(
        content(
            &mut app,
            activity_change(london, "suspend-neighbor", "suspended")
        )["status"],
        "activity_set"
    );
    assert_eq!(content(&mut app, input.clone()), before);
    input.as_object_mut().unwrap().remove("base_revision");
    input["command"] = json!("save");
    input["token"] = before["token"].clone();
    let saved = content(&mut app, input);
    assert_eq!(saved["status"], "repertoires", "{saved}");
}

#[test]
fn repertoire_suspension_never_reattributes_a_saved_game_choice() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("suspend-chosen-book.sqlite");
    let mut app = open(
        &db,
        Arc::new(AtomicI32::new(20_000)),
        IntroductionLimits::default(),
        0,
    );
    let (_, created) = preview_and_save(&mut app, SHORT, "Italian", None);
    let italian = created["repertoires"][0]["id"].as_str().unwrap();
    let (_, books) = preview_and_save(&mut app, "1. d4 d5 2. Bf4 *", "London", None);
    let london = books["repertoires"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "London")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let played = "1. c4 e5";
    let report = date_only_import(&mut app, "compare-overlap", "2024.10.01", played);
    let result = &report["results"][0];
    let choice = &result["choices"][0];
    let candidates = choice["candidates"].as_array().unwrap();
    let target = |book: &str| {
        let id = candidates
            .iter()
            .find(|candidate| candidate["repertoire_id"] == book)
            .unwrap()["target_id"]
            .as_str()
            .unwrap();
        TargetId::from(id)
    };
    let italian_target = target(italian);
    let london_target = target(london);
    assert_eq!(candidates.len(), 2, "{choice}");

    let suspend = activity_change(london, "suspend-unresolved", "suspended");
    assert_eq!(content(&mut app, suspend)["status"], "activity_set");
    let history = app.game_history().unwrap();
    assert_eq!(
        history["results"][0]["correction_choice_status"],
        "compare_again"
    );
    assert_eq!(
        history["results"][0]["choices"][0]["candidates"]
            .as_array()
            .unwrap(),
        candidates
    );
    let resume = activity_change(london, "resume-unresolved", "active");
    assert_eq!(content(&mut app, resume)["status"], "activity_set");
    assert!(app.game_history().unwrap()["results"][0]["correction_choice_status"].is_null());

    app.choose_game_correction(serde_json::from_value(json!({
        "command_id":"choose-london", "dataset_id":"opening-proof", "learner":"learner",
        "source":result["source"], "game_id":result["game_id"], "comparison_id":result["comparison_id"],
        "ply":choice["ply"], "target_id":london_target,
    })).unwrap()).unwrap();
    let store = SqliteStore::open(&db).unwrap();
    let misses = || {
        let facts = store
            .game_relevance(&"opening-proof".into(), tempo_store::REACH_MODEL_KEY)
            .unwrap();
        (facts[&italian_target].misses, facts[&london_target].misses)
    };
    assert_eq!(misses(), (0, 1));
    let before = app.snapshot().unwrap();
    assert_eq!(
        content(
            &mut app,
            activity_change(london, "suspend-chosen", "suspended")
        )["status"],
        "activity_set"
    );
    assert_eq!(app.reconcile_choice_encounters().unwrap().repaired, 0);
    assert_eq!(misses(), (0, 1));
    date_only_import(&mut app, "reprocess-suspended", "2024.10.01", played);
    assert_eq!(misses(), (0, 1));
    assert_eq!(
        content(&mut app, activity_change(london, "resume-chosen", "active"))["status"],
        "activity_set"
    );
    assert_eq!(misses(), (0, 1));
    let after = app.snapshot().unwrap();
    assert_eq!(after.assessments, before.assessments);
    assert_eq!(after.learning, before.learning);
}
