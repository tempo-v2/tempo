//! Qualification with the real SQLite adapter, legal grader, and upstream FSRS.
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
};
use tempo::{Clock, Components, CounterIds, IntroductionLimits, Tempo};
use tempo_chess::{Catalog, LegalSequenceGrader};
use tempo_learning::{DailyLearning, LearningPolicy};
use tempo_store::{SqliteStore, Store};
use tempo_types::{StudyDay, StudyTime, TargetId};

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
fn portable_choice_attribution_survives_restore_and_reimport() {
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
    // The baseline native observation recovery was separately executed and is
    // recorded in portable-choice-proof.log. Preserve the selected-result
    // identity here without depending on internal SQLite table layout.
    let parent_id = result["comparison_id"].as_str().unwrap();
    let child_id = blake3::hash(json!([parent_id, 1usize, corrected]).to_string().as_bytes())
        .to_hex()
        .to_string();
    assert_eq!(
        accepted.results[0].comparison_id.as_deref(),
        Some(child_id.as_str())
    );
    eprintln!(
        "native no-new-grade choice retains its selected-result comparison identity; no new chosen failure row"
    );
    let package = source.export(&tempo::JsonSnapshotCodec).unwrap();
    let payload: Value = serde_json::from_str(&package).unwrap();
    eprintln!("exported portable format {}", payload["schema_version"]);
    assert_eq!(
        payload["resolved_game_choices"].as_array().unwrap().len(),
        1
    );
    let before_learning = source.snapshot().unwrap().learning;
    let restored_db = directory.path().join("restored-choice.sqlite");
    let mut restored = open(&restored_db, day.clone(), IntroductionLimits::default(), 0);
    restored
        .restore(
            "new-choice-dataset".into(),
            &package,
            &tempo::JsonSnapshotCodec,
        )
        .unwrap();
    drop(restored);
    let mut restored = open(&restored_db, day.clone(), IntroductionLimits::default(), 1);
    let reimported = date_only_import(&mut restored, "reimported", "2024.10.06", "1. c4 e5");
    assert_eq!(reimported["results"][0]["effect"], "already_addressed");
    assert!(reimported["results"][0].get("choices").is_none());
    assert_eq!(restored.snapshot().unwrap().learning, before_learning);
    let relevance = SqliteStore::open(&restored_db)
        .unwrap()
        .game_relevance(&restored.snapshot().unwrap().dataset, "")
        .unwrap();
    eprintln!(
        "portable no-new-grade choice after restore/reimport: exposure={}, misses={}",
        relevance[&corrected].exposure, relevance[&corrected].misses
    );
    assert_eq!(relevance[&corrected].exposure, 1);
    assert_eq!(
        relevance[&corrected].misses, 1,
        "New exports must carry accepted selection attribution independently of a grade"
    );

    let legacy_db = directory.path().join("old-choice.sqlite");
    let mut old = open(&legacy_db, day, IntroductionLimits::default(), 0);
    let mut malformed = payload.clone();
    malformed["resolved_game_choices"][0]["attribution"]["answer_version"] = json!(0);
    assert!(
        old.restore(
            "rejected".into(),
            &malformed.to_string(),
            &tempo::JsonSnapshotCodec
        )
        .is_err()
    );
    assert!(old.snapshot().unwrap().catalog.items.is_empty());

    let mut legacy = payload;
    legacy["schema_version"] = json!(4);
    legacy["resolved_game_choices"][0]
        .as_object_mut()
        .unwrap()
        .remove("attribution");
    old.restore(
        "old-choice-dataset".into(),
        &legacy.to_string(),
        &tempo::JsonSnapshotCodec,
    )
    .unwrap();
    let reimported = date_only_import(&mut old, "reimported", "2024.10.06", "1. c4 e5");
    assert_eq!(reimported["results"][0]["effect"], "already_addressed");
    assert!(reimported["results"][0].get("choices").is_none());
    let store = SqliteStore::open(&legacy_db).unwrap();
    let dataset = old.snapshot().unwrap().dataset;
    assert_eq!(
        store.game_relevance(&dataset, "").unwrap()[&corrected].misses,
        0
    );
    assert_eq!(old.snapshot().unwrap().learning, before_learning);
}

#[test]
fn old_choice_stays_closed_without_version_upgrade_or_reset_regrading() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("versioned-choice.sqlite");
    let day = Arc::new(AtomicI32::new(20_004));
    let mut app = open(&db, day, IntroductionLimits::default(), 0);
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
