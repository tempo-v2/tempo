//! Qualification with the real SQLite adapter, legal grader, and upstream FSRS.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
};
use tempo::{Clock, Components, CounterIds, IntroductionLimits, Tempo, wire::StudyCommand};
use tempo_chess::{
    Catalog, LearnerColor, LegalSequenceGrader, compile_repertoire, plan_repertoire_replacement,
};
use tempo_learning::{DailyLearning, LearningPolicy};
use tempo_store::{
    BookProjection, Commit, ModelAuthority, ProjectionBook, ProjectionScope, REACH_MODEL_KEY,
    SqliteStore, Store, StoreError, Write, model_targets,
};
use tempo_types::{DatasetId, StudyDay, StudyTime, TargetId};

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

fn play_activity(app: &mut Tempo, activity: &Value, command: &str, id: &str, uci: &str) -> Value {
    let mut answer = activity_command(command, activity, id);
    answer["answer"] = json!({"uci": uci});
    study(app, answer)
}

fn publish(
    store: &mut SqliteStore,
    dataset: &DatasetId,
    book: &ProjectionBook,
    complete: bool,
    reach: &BTreeMap<TargetId, u32>,
) -> Result<(), StoreError> {
    store.replace_game_model_reach(
        dataset,
        BookProjection {
            repertoire_id: &book.repertoire_id,
            model: REACH_MODEL_KEY,
            input_key: &book.input_key,
            complete,
            reach,
        },
    )
}

#[test]
fn display_rename_must_preserve_modeled_due_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("before.sqlite");
    let day = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits::default();
    let mut app = open(&db, day.clone(), limits, 0);
    let (_, saved) = preview_and_save(&mut app, SHORT, "Before", None);
    let repertoire = saved["repertoires"][0]["id"].as_str().unwrap().to_owned();
    for index in 0..2 {
        let card = study(&mut app, json!({"command":"next"}));
        assert_eq!(card["activity"], "teach");
        let reply = play_activity(
            &mut app,
            &card,
            "acknowledge",
            &format!("teach-{index}"),
            card["lesson"]["move_uci"].as_str().unwrap(),
        );
        assert_eq!(reply["status"], "complete");
    }
    let snapshot = app.snapshot().unwrap();
    drop(app);
    day.store(20_001, Ordering::Relaxed);
    let baseline_db = dir.path().join("baseline.sqlite");
    std::fs::copy(&db, &baseline_db).unwrap();
    let mut baseline = open(&baseline_db, day.clone(), limits, 1);
    let random_first = study(&mut baseline, json!({"command":"next"}))["target_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let favored = snapshot
        .catalog
        .items
        .keys()
        .find(|id| id.0 != random_first)
        .unwrap()
        .clone();
    let mut store = SqliteStore::open(&db).unwrap();
    let book = store
        .projection_books(&snapshot.dataset, ProjectionScope::AllVerified)
        .unwrap()
        .remove(0);
    let reach = std::collections::BTreeMap::from([
        (favored.clone(), 900_000),
        (TargetId(random_first.clone()), 1_000),
    ]);
    publish(&mut store, &snapshot.dataset, &book, true, &reach).unwrap();
    assert_eq!(
        store
            .game_relevance(&snapshot.dataset, REACH_MODEL_KEY)
            .unwrap()
            .len(),
        2
    );
    drop(store);
    let renamed_db = dir.path().join("renamed.sqlite");
    std::fs::copy(&db, &renamed_db).unwrap();
    let tactics_db = dir.path().join("tactics.sqlite");
    std::fs::copy(&db, &tactics_db).unwrap();
    let mut ranked = open(&db, day.clone(), limits, 1);
    let ranked_first = study(&mut ranked, json!({"command":"next"}));
    assert_eq!(ranked_first["target_id"], favored.0);
    let mut renamed = open(&renamed_db, day.clone(), limits, 1);
    let reply = content(
        &mut renamed,
        json!({"command":"rename","dataset_id":"opening-proof","repertoire_id":repertoire,"name":"After"}),
    );
    assert_eq!(reply["repertoires"][0]["name"], "After");
    let remaining = SqliteStore::open(&renamed_db)
        .unwrap()
        .game_relevance(&snapshot.dataset, REACH_MODEL_KEY)
        .unwrap();
    assert_eq!(remaining.len(), 2);
    let renamed_first = study(&mut renamed, json!({"command":"next"}));
    eprintln!(
        "modeled_count_before=2 modeled_count_after_rename={} random_first={} modeled_first={} renamed_first={}",
        remaining.len(),
        random_first,
        ranked_first["target_id"],
        renamed_first["target_id"]
    );
    assert_eq!(
        renamed_first["target_id"], ranked_first["target_id"],
        "A display-only rename must preserve review selection"
    );
    let mut tactics_store = SqliteStore::open(&tactics_db).unwrap();
    let current = tactics_store.snapshot().unwrap();
    let mut catalog = current.catalog;
    let mut tactic = tempo_chess::demo_catalog()
        .items
        .remove(&TargetId::from("t1"))
        .unwrap();
    tactic.status = tempo_chess::ContentStatus::Quarantined;
    let target = tactic.target.clone();
    catalog.items.insert(target.clone(), tactic);
    tactics_store
        .commit(Commit {
            expected_dataset: current.dataset.clone(),
            expected_revision: current.revision,
            writes: vec![
                Write::Catalog(catalog),
                Write::Learning {
                    target,
                    state: Default::default(),
                },
            ],
        })
        .unwrap();
    assert_eq!(
        tactics_store
            .game_relevance(&current.dataset, REACH_MODEL_KEY)
            .unwrap()
            .len(),
        2
    );
    let mut tactics = open(&tactics_db, day, limits, 1);
    assert_eq!(
        study(&mut tactics, json!({"command":"next"}))["target_id"],
        ranked_first["target_id"]
    );
}

#[test]
fn equivalent_documents_keep_authority_until_only_one_books_route_changes() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("semantic.sqlite");
    let day = Arc::new(AtomicI32::new(20_000));
    let limits = IntroductionLimits::default();
    let mut app = open(&db, day.clone(), limits, 0);
    let original = "1. Nf3 Nf6 (1... g6 2. g3 Nf6) 2. g3 g6 3. Bg2 *";
    let new_route = "1. Nf3 Nf6 (1... g6 2. g3 Nc6) 2. g3 g6 3. Bg2 *";
    let (original_preview, saved) = preview_and_save(&mut app, original, "First", None);
    let book_id = saved["repertoires"][0]["id"].as_str().unwrap().to_owned();
    preview_and_save(&mut app, SHORT, "Second", None);
    let dataset = app.snapshot().unwrap().dataset;
    let mut store = SqliteStore::open(&db).unwrap();
    let mut books = store
        .projection_books(&dataset, ProjectionScope::AllVerified)
        .unwrap();
    let old = books.remove(
        books
            .iter()
            .position(|book| book.repertoire_id == book_id)
            .unwrap(),
    );
    let other = books.remove(0);
    let reach = old
        .targets
        .keys()
        .cloned()
        .map(|target| (target, 380_000))
        .collect();
    let other_reach = other
        .targets
        .keys()
        .cloned()
        .map(|target| (target, 0))
        .collect();
    publish(&mut store, &dataset, &old, true, &reach).unwrap();
    publish(&mut store, &dataset, &other, true, &other_reach).unwrap();
    publish(&mut store, &dataset, &old, false, &BTreeMap::new()).unwrap();
    let status = store
        .game_model_projection_status(&dataset)
        .unwrap()
        .unwrap();
    assert_eq!((status.complete_current, status.partial_current), (2, 0));

    let equivalent = format!("{original}\n\n{{Equivalent duplicate}} {original}");
    preview_and_save(&mut app, &equivalent, "First", Some(&book_id));
    assert_eq!(
        store
            .game_model_projection_status(&dataset)
            .unwrap()
            .unwrap()
            .complete_current,
        2
    );
    assert!(
        store
            .projection_books(&dataset, ProjectionScope::MissingOrPartial)
            .unwrap()
            .is_empty()
    );
    let before = store.content_state().unwrap();
    let changed =
        compile_repertoire(new_route, &book_id, "First", LearnerColor::White, "fixture").unwrap();
    let changed = plan_repertoire_replacement(&before.catalog, &changed, &book_id)
        .unwrap()
        .catalog;
    assert_eq!(
        model_targets(&before.catalog, &book_id),
        model_targets(&changed, &book_id)
    );
    store
        .commit(Commit {
            expected_dataset: dataset.clone(),
            expected_revision: before.revision,
            writes: vec![Write::Catalog(changed)],
        })
        .unwrap();
    let status = store
        .game_model_projection_status(&dataset)
        .unwrap()
        .unwrap();
    assert_eq!((status.complete_current, status.unavailable), (1, 1));
    assert!(matches!(
        publish(&mut store, &dataset, &old, true, &reach),
        Err(StoreError::Conflict(_))
    ));
    assert_eq!(
        store
            .game_relevance(&dataset, REACH_MODEL_KEY)
            .unwrap()
            .values()
            .filter_map(|result| result.modeled_millionths)
            .collect::<Vec<_>>(),
        vec![0; other.targets.len()]
    );

    drop(app);
    let mut app = open(&db, day, limits, 1);
    app.bootstrap_model_inputs().unwrap();
    assert_eq!(
        store.repertoire_document(&book_id).unwrap().model_authority,
        ModelAuthority::Unavailable
    );

    preview_and_save(&mut app, new_route, "First", Some(&book_id));
    let new = store
        .projection_books(&dataset, ProjectionScope::MissingOrPartial)
        .unwrap()
        .remove(0);
    assert_ne!(new.input_key, old.input_key);
    publish(&mut store, &dataset, &new, true, &reach).unwrap();
    let mut replay = book_request("save", original, "First", None);
    replay["token"] = original_preview["token"].clone();
    assert_eq!(content(&mut app, replay), saved);
    assert!(matches!(
        publish(&mut store, &dataset, &old, true, &reach),
        Err(StoreError::Conflict(_))
    ));
    assert_eq!(
        store
            .game_model_projection_status(&dataset)
            .unwrap()
            .unwrap()
            .complete_current,
        2
    );
}

#[test]
fn issued_reason_survives_new_model_order_and_old_work_cannot_cross_restore() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("issued.sqlite");
    let day = Arc::new(AtomicI32::new(20_000));
    let mut app = open(&path, day.clone(), IntroductionLimits::default(), 0);
    preview_and_save(&mut app, "1. e4 e5 2. Nf3 Nc6 3. Bb5 *", "Three", None);
    for teach in 0..3 {
        let card = study(&mut app, json!({"command":"next"}));
        assert_eq!(card["activity"], "teach");
        let reply = play_activity(
            &mut app,
            &card,
            "acknowledge",
            &format!("teach-{teach}"),
            card["lesson"]["move_uci"].as_str().unwrap(),
        );
        assert_eq!(reply["status"], "complete");
    }
    day.store(20_001, Ordering::Relaxed);
    let mut store = SqliteStore::open(&path).unwrap();
    let dataset = app.snapshot().unwrap().dataset;
    let book = store
        .projection_books(&dataset, ProjectionScope::AllVerified)
        .unwrap()
        .remove(0);
    let targets: Vec<_> = book.targets.keys().cloned().collect();
    let scores = |weights: [u32; 3]| targets.iter().cloned().zip(weights).collect();
    publish(
        &mut store,
        &dataset,
        &book,
        true,
        &scores([900_000, 600_000, 100_000]),
    )
    .unwrap();
    let issued = study(&mut app, json!({"command":"next"}));
    assert_eq!(issued["target_id"], targets[0].0);
    let attempt = issued["attempt_id"].as_str().unwrap().into();
    let reason = app.study_relevance_reason(&dataset, &attempt).unwrap();
    assert_eq!(
        reason["reason"],
        "Modeled reach · 90.0% under Maia Rapid 1500."
    );

    publish(
        &mut store,
        &dataset,
        &book,
        true,
        &scores([10_000, 200_000, 800_000]),
    )
    .unwrap();
    assert_eq!(
        study(&mut app, json!({"command":"next"}))["attempt_id"],
        issued["attempt_id"]
    );
    assert_eq!(
        app.study_relevance_reason(&dataset, &attempt).unwrap(),
        reason
    );
    let response = &book.targets[&targets[0]].1.decisions[0].expected.uci;
    let saved = play_activity(&mut app, &issued, "answer", "ranked-answer", response);
    assert_eq!(saved["status"], "complete");
    assert_eq!(
        study(&mut app, json!({"command":"next"}))["target_id"],
        targets[2].0
    );

    let current = store.snapshot().unwrap();
    let fresh = DatasetId::from("fresh-model-dataset");
    store
        .commit(Commit {
            expected_dataset: dataset.clone(),
            expected_revision: current.revision,
            writes: vec![Write::Restore {
                dataset: fresh.clone(),
                catalog: current.catalog,
                learning: current.learning,
                policy: current.policy,
            }],
        })
        .unwrap();
    assert!(matches!(
        publish(
            &mut store,
            &dataset,
            &book,
            true,
            &scores([900_000, 600_000, 100_000])
        ),
        Err(StoreError::Conflict(_))
    ));
    assert!(
        store
            .game_relevance(&fresh, REACH_MODEL_KEY)
            .unwrap()
            .is_empty()
    );
    let status = store.game_model_projection_status(&fresh).unwrap().unwrap();
    assert_eq!((status.unavailable, status.complete_current), (1, 0));
}
