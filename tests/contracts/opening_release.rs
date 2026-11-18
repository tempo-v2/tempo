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
use tempo_chess::{Catalog, ImportRequest, LearnerColor, LegalSequenceGrader, compile_repertoire};
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
        ids: Box::new(CounterIds(u64::from(generation) * 1_000)),
        learning: Box::new(learning),
        ..Components::fixtures()
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
    assert_eq!(scoped.catalog.items.len(), 1);
    assert_eq!(scoped.learning.len(), 1);
    assert_eq!(scoped.attempts.len(), 1);
    assert!(
        scoped.assessments.is_empty()
            && scoped.receipts.is_empty()
            && scoped.content_history.is_empty()
    );
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
    assert!(
        after
            .learning
            .values()
            .any(|state| state.game_teaching_relevance)
    );
    assert!(
        after
            .learning
            .values()
            .all(|state| state.scheduler_memory.is_none())
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
fn real_daily_tests_address_covered_game_misses_and_preserve_newer_work() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("daily-games.sqlite");
    let today = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits::default();
    let mut app = open(&db, today.clone(), limits, 0);
    let (_, saved) = preview_and_save(&mut app, "1. e4 *", "White", None);
    let repertoire = saved["repertoires"][0]["id"].as_str().unwrap();
    let lesson = study(&mut app, json!({"command":"next"}));
    let target = TargetId::from(lesson["target_id"].as_str().unwrap());
    let mut ack = activity_command("acknowledge", &lesson, "teach-game-target");
    ack["answer"] = json!({"uci":"e2e4"});
    assert_eq!(study(&mut app, ack)["assessment"], Value::Null);
    today.store(20_001, Ordering::Relaxed);
    let first = study(&mut app, json!({"command":"next"}));
    let mut answer = activity_command("answer", &first, "original-scheduled-test");
    answer["answer"] = json!({"uci":"e2e4"});
    assert_eq!(study(&mut app, answer)["assessment"], "good");

    today.store(20_002, Ordering::Relaxed);
    let before = app.snapshot().unwrap();
    assert!(before.learning[&target].due.unwrap() > StudyDay(20_002));
    assert_eq!(app.study_progress(None).unwrap().unwrap().due, 0);
    let report = dated_game(&mut app, repertoire, "2024.10.05", "1. d4 d5", "12:00:00");
    assert_eq!(
        report["results"][0]["effect"], "requested_review",
        "{report}"
    );
    let staged = app.snapshot().unwrap();
    assert_eq!(staged.assessments, before.assessments);
    assert_eq!(
        staged.learning[&target].scheduler_memory,
        before.learning[&target].scheduler_memory
    );
    assert_eq!(staged.learning[&target].due, before.learning[&target].due);
    assert_eq!(app.study_progress(None).unwrap().unwrap().due, 1);
    drop(app);
    let mut app = open(&db, today.clone(), limits, 1);
    let requested = study(&mut app, json!({"command":"next"}));
    assert_eq!(requested["activity"], "test");
    assert_eq!(requested["target_id"], target.0);
    assert!(requested.get("lesson").is_none());
    assert!(requested.get("revealed_line").is_none());
    assert!(!requested.to_string().contains("expected"));

    // This game happened after issuance. Importing during the active Test
    // must leave the Test visible and preserve the newer obligation afterward.
    today.store(20_003, Ordering::Relaxed);
    let later = dated_game(&mut app, repertoire, "2024.10.06", "1. c4 c5", "12:00:00");
    assert_eq!(later["results"][0]["effect"], "requested_review");
    assert_eq!(
        study(&mut app, json!({"command":"next"}))["attempt_id"],
        requested["attempt_id"]
    );
    let mut miss = activity_command("answer", &requested, "requested-test-miss");
    miss["answer"] = json!({"uci":"d2d4"});
    let result = study(&mut app, miss.clone());
    assert_eq!(result["assessment"], "again");
    assert_eq!(study(&mut app, miss), result);
    let mut correct = activity_command("answer", &result, "finish-correction");
    correct["answer"] = json!({"uci":"e2e4"});
    assert_eq!(study(&mut app, correct)["status"], "complete");
    let after = app.snapshot().unwrap();
    assert_eq!(after.assessments.len(), 2);
    assert_eq!(
        after.learning[&target].game_review.unwrap().actionable_day,
        StudyDay(20_004)
    );
    assert_eq!(app.study_progress(None).unwrap().unwrap().due, 0);
    assert_eq!(study(&mut app, json!({"command":"next"}))["done"], true);
    let replay = dated_game(&mut app, repertoire, "2024.10.05", "1. d4 d5", "12:00:00");
    assert_eq!(replay["duplicates"], 1);
    assert_eq!(replay["results"][0]["effect"], "already_addressed");
    assert_eq!(app.snapshot().unwrap(), after);

    today.store(20_004, Ordering::Relaxed);
    let test = study(&mut app, json!({"command":"next"}));
    let mut answer = activity_command("answer", &test, "requested-test-good");
    answer["answer"] = json!({"uci":"e2e4"});
    assert_eq!(study(&mut app, answer)["assessment"], "good");
    assert!(
        app.snapshot().unwrap().learning[&target]
            .game_review
            .is_none()
    );
    assert_eq!(
        app.game_history().unwrap()["results"][0]["effect"],
        "already_addressed"
    );
    let package = app.export(&tempo::JsonSnapshotCodec).unwrap();
    let mut restored = open(&directory.path().join("restored.sqlite"), today, limits, 0);
    restored
        .restore("new-dataset".into(), &package, &tempo::JsonSnapshotCodec)
        .unwrap();
    let replay = dated_game(
        &mut restored,
        repertoire,
        "2024.10.06",
        "1. c4 c5",
        "12:00:00",
    );
    assert_eq!(replay["results"][0]["effect"], "already_addressed");
    assert!(
        restored.snapshot().unwrap().learning[&target]
            .game_review
            .is_none()
    );
}

#[test]
fn a_deeper_game_miss_prioritizes_only_the_current_frontier_and_keeps_teaching_ungraded() {
    let directory = tempfile::tempdir().unwrap();
    let today = Arc::new(AtomicI32::new(20_004));
    let mut app = open(
        &directory.path().join("frontier.sqlite"),
        today,
        IntroductionLimits {
            repertoire: 1,
            tactics: 0,
        },
        0,
    );
    preview_and_save(&mut app, "1. c4 *", "A neutral route", None);
    let (_, saved) = preview_and_save(&mut app, "1. e4 e5 2. Nf3 *", "Relevant route", None);
    let repertoire = saved["repertoires"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "Relevant route")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let report = dated_game(
        &mut app,
        repertoire,
        "2024.10.06",
        "1. e4 e5 2. Bc4",
        "12:00:00",
    );
    assert_eq!(report["results"][0]["effect"], "prioritized_teaching");
    let frontier = study(&mut app, json!({"command":"next"}));
    assert_eq!(frontier["activity"], "teach");
    assert_eq!(frontier["lesson"]["move_uci"], "e2e4");
    let mut acknowledge = activity_command("acknowledge", &frontier, "frontier-introduction");
    acknowledge["answer"] = json!({"uci":"e2e4"});
    assert_eq!(study(&mut app, acknowledge)["assessment"], Value::Null);
    assert_eq!(study(&mut app, json!({"command":"next"}))["done"], true);
    assert!(app.snapshot().unwrap().assessments.is_empty());
}

#[test]
fn completed_lichess_pgn_requests_an_ordinary_test_and_replays_after_repertoire_edit() {
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
        "requested_review"
    );
    let requested = study(&mut app, json!({"command":"next"}));
    let mut test = activity_command("answer", &requested, "real-game-resolved");
    test["answer"] = json!({"uci":"e2e4"});
    assert_eq!(study(&mut app, test)["assessment"], "good");
    assert_eq!(
        app.game_history().unwrap()["results"][0]["effect"],
        "already_addressed"
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
        "stale_content"
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
    assert_eq!(app.snapshot().unwrap().assessments.len(), 3);
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
        for row in rows["results"].as_array().unwrap() {
            assert_eq!(row["effect"], "requested_review");
        }

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
    }
}
