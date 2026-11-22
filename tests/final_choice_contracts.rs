//! Qualification with the real SQLite adapter, legal grader, and upstream FSRS.
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
};
use tempo::{Clock, CounterIds, DailyComponents, IntroductionLimits, Tempo};
use tempo_chess::{Catalog, LegalSequenceGrader};
use tempo_learning::{DailyLearning, LearningPolicy};
use tempo_store::{ChoiceAttribution, SqliteStore, Store};
use tempo_types::{ContentVersion, StudyDay, StudyTime, TargetId};

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
    };
    Tempo::daily(Box::new(store), components, limits, [42; 32], 10).unwrap()
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

fn choose(app: &mut Tempo, original: &Value, choice: &Value, command_id: &str) {
    let result = &original["results"][0];
    let target = &choice["candidates"][0]["target_id"];
    let report = app.choose_game_correction(serde_json::from_value(json!({
        "command_id":command_id, "dataset_id":"opening-proof", "learner":"learner",
        "source":result["source"], "game_id":result["game_id"], "comparison_id":result["comparison_id"],
        "ply":choice["ply"], "target_id":target
    })).unwrap()).unwrap();
    assert!(matches!(
        serde_json::to_value(report).unwrap()["results"][0]["effect"].as_str(),
        Some("again_applied" | "already_addressed")
    ));
}

#[test]
fn startup_recovers_a_no_grade_choice_written_entirely_under_schema9() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("choice.sqlite");
    // Schema 9 Tempo be6ca8f accepted the choice after an existing automatic Again.
    // Schema 10 Tempo 598c2d3 reopened and reimported the same synthetic game.
    std::fs::write(&db, include_bytes!("fixtures/schema9-choice.sqlite")).unwrap();
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/schema9-choice.json")).unwrap();
    let command = || {
        serde_json::from_value::<tempo::game_evidence::ChooseGameCorrection>(
            fixture["command"].clone(),
        )
        .unwrap()
    };
    let choice = command();
    let position = fixture["position"].as_str().unwrap();
    let store = SqliteStore::open(&db).unwrap();
    let migration = store.choice_migration_report().unwrap();
    assert_eq!(
        (
            migration.recovered,
            migration.unattributed,
            migration.conflicting
        ),
        (1, 0, 0)
    );
    let dataset = &choice.dataset_id;
    let choices = || {
        store
            .game_choices(dataset, &choice.source, &choice.game_id)
            .unwrap()
    };
    assert_eq!(
        choices()[position],
        Some(ChoiceAttribution {
            target: choice.target_id.clone(),
            answer_version: ContentVersion(1)
        })
    );
    let relevance = || store.game_relevance(dataset, "").unwrap()[&choice.target_id];
    assert_eq!((relevance().exposure, relevance().misses), (1, 0));
    let learning_before = store.snapshot().unwrap().learning;
    let failures_before = store.game_failures(dataset).unwrap();
    assert_eq!(failures_before.len(), 1);
    assert_eq!(failures_before[0].chosen_position, None);
    let receipt = || {
        store
            .game_receipt(
                dataset,
                &choice.command_id,
                fixture["fingerprint"].as_str().unwrap(),
            )
            .unwrap()
            .unwrap()
    };

    let startup = |generation| {
        let day = Arc::new(AtomicI32::new(20_004));
        let mut app = open(&db, day, IntroductionLimits::default(), generation);
        app.reconcile_legacy_game_requests().unwrap();
        let repair = app.reconcile_choice_encounters().unwrap();
        (app, repair)
    };
    let (mut app, repair) = startup(1);
    assert_eq!(
        (repair.examined, repair.repaired, repair.unresolved),
        (1, 1, 0)
    );
    let replay = app.choose_game_correction(command()).unwrap();
    assert_eq!(
        serde_json::to_value(replay).unwrap(),
        fixture["accepted_reply"]
    );
    assert_eq!((relevance().exposure, relevance().misses), (1, 1));
    drop(app);

    let (mut reopened, repair) = startup(2);
    assert_eq!((repair.repaired, repair.unresolved), (0, 0));
    date_only_import(
        &mut reopened,
        "reimport-after-repair",
        "2024.10.06",
        "1. c4 e5",
    );
    assert_eq!((relevance().exposure, relevance().misses), (1, 1));
    assert_eq!(store.snapshot().unwrap().learning, learning_before);
    assert_eq!(store.game_failures(dataset).unwrap(), failures_before);
    assert_eq!(serde_json::to_value(receipt()).unwrap(), fixture["receipt"]);
}

#[test]
fn choice_attribution_survives_reopen_and_reimport() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("native-choice.sqlite");
    let day = Arc::new(AtomicI32::new(20_004));
    let mut source = open(&db, day.clone(), IntroductionLimits::default(), 0);
    preview_and_save(&mut source, "1. e4 *", "First", None);
    date_only_import(&mut source, "automatic", "2024.10.06", "1. c4 e5");
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
    let accepted = source.choose_game_correction(serde_json::from_value(json!({
        "command_id":"choose-no-new-grade", "dataset_id":"opening-proof", "learner":"learner",
        "source":result["source"], "game_id":result["game_id"], "comparison_id":result["comparison_id"],
        "ply":1, "target_id":corrected
    })).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(&accepted).unwrap()["results"][0]["effect"],
        "already_addressed"
    );
    let failure_rows = SqliteStore::open(&db)
        .unwrap()
        .game_failures(&source.snapshot().unwrap().dataset)
        .unwrap();
    assert_eq!(failure_rows.len(), 1);
    assert_eq!(failure_rows[0].chosen_position, None);
    let parent_id = result["comparison_id"].as_str().unwrap();
    let child_id = blake3::hash(json!([parent_id, 1usize, corrected]).to_string().as_bytes())
        .to_hex()
        .to_string();
    assert_eq!(
        accepted.results[0].comparison_id.as_deref(),
        Some(child_id.as_str())
    );
    let before_learning = source.snapshot().unwrap().learning;
    drop(source);
    let mut reopened = open(&db, day, IntroductionLimits::default(), 1);
    let reimported = date_only_import(&mut reopened, "reimported", "2024.10.06", "1. c4 e5");
    assert_eq!(reimported["results"][0]["effect"], "already_addressed");
    assert!(reimported["results"][0].get("choices").is_none());
    assert_eq!(reopened.snapshot().unwrap().learning, before_learning);
    let relevance = SqliteStore::open(&db)
        .unwrap()
        .game_relevance(&reopened.snapshot().unwrap().dataset, "")
        .unwrap();
    assert_eq!(
        (relevance[&corrected].exposure, relevance[&corrected].misses),
        (1, 1)
    );
}

#[test]
fn old_choice_stays_closed_without_version_upgrade_or_reset_regrading() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("versioned-choice.sqlite");
    let day = Arc::new(AtomicI32::new(20_004));
    let mut app = open(&db, day.clone(), IntroductionLimits::default(), 0);
    preview_and_save(&mut app, "1. e4 e5 2. Nf3 *", "First", None);
    preview_and_save(&mut app, "1. e4 e5 2. Bc4 *", "Second", None);
    let game = date_only_import(&mut app, "ambiguous", "2024.10.06", "1. e4 e5 2. Nc3");
    let choice = &game["results"][0]["choices"][0];
    let target = TargetId::from(choice["candidates"][0]["target_id"].as_str().unwrap());
    choose(&mut app, &game, choice, "choose-once");
    let prior = app.snapshot().unwrap();
    let item = &prior.catalog.items[&target];
    let tempo_chess::ContentKind::Repertoire { repertoire, .. } = &item.definition.kind else {
        panic!()
    };
    let rep = repertoire.clone();
    let name = item.label.clone();
    let store = SqliteStore::open(&db).unwrap();
    let dataset = prior.dataset;
    let old_version = item.version;
    assert_eq!(
        store.game_relevance(&dataset, "").unwrap()[&target].misses,
        1
    );
    preview_and_save(&mut app, "1. e4 e5 2. d3 *", &name, Some(&rep));
    let changed = app.snapshot().unwrap();
    assert_ne!(changed.catalog.items[&target].version, old_version);
    date_only_import(
        &mut app,
        "new-answer-reimport",
        "2024.10.06",
        "1. e4 e5 2. Nc3",
    );
    assert_eq!(
        store.game_relevance(&dataset, "").unwrap()[&target].misses,
        0
    );
    assert_eq!(app.snapshot().unwrap().learning, changed.learning);
    preview_and_save(&mut app, "1. e4 *", &name, Some(&rep));
    assert!(!app.snapshot().unwrap().catalog.items[&target].active());
    drop(app);
    let mut app = open(&db, day, IntroductionLimits::default(), 1);
    let snapshot = app.snapshot().unwrap();
    let retired_item = &snapshot.catalog.items[&target];
    assert!(!retired_item.active());
    assert!(old_version.0 < retired_item.version.0);
    let result = &game["results"][0];
    let choices = store
        .game_choices(
            &dataset,
            result["source"].as_str().unwrap(),
            result["game_id"].as_str().unwrap(),
        )
        .unwrap();
    let saved = choices[choice["position"].as_str().unwrap()]
        .as_ref()
        .unwrap();
    assert_eq!(saved.target, target);
    assert_eq!(saved.answer_version, old_version);

    date_only_import(
        &mut app,
        "retired-reimport",
        "2024.10.06",
        "1. e4 e5 2. Nc3",
    );
    assert_eq!(store.game_failures(&dataset).unwrap().len(), 1);
    preview_and_save(&mut app, "1. e4 e5 2. d3 *", &name, Some(&rep));
    assert!(app.snapshot().unwrap().catalog.items[&target].active());
    let reset = content(
        &mut app,
        json!({"command":"reset_progress","dataset_id":dataset,"repertoire_id":rep,"command_id":"reset-after-choice"}),
    );
    assert_eq!(reset["status"], "progress_reset");
    let reset_learning = app.snapshot().unwrap().learning;
    let after = date_only_import(
        &mut app,
        "post-reset-reimport",
        "2024.10.06",
        "1. e4 e5 2. Nc3",
    );
    assert!(after["results"][0].get("choices").is_none());
    assert_eq!(app.snapshot().unwrap().learning, reset_learning);
    let failures = store.game_failures(&dataset).unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].version, old_version);
    assert_eq!(
        store.game_relevance(&dataset, "").unwrap()[&target].misses,
        0
    );
    eprintln!(
        "guard passed: selected version1 remains historic through new answer version2, retirement/reactivation and reset; no fresh Game Again, no reopened choice"
    );
}
