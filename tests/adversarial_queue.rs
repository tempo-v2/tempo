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
fn accepted_already_addressed_choice_keeps_its_miss_after_reprocessing() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("choice-relevance.sqlite");
    let day = Arc::new(AtomicI32::new(20_004));
    let mut app = open(&db, day, IntroductionLimits::default(), 0);
    preview_and_save(&mut app, "1. e4 *", "First", None);
    let automatic = date_only_import(&mut app, "automatic", "2024.10.06", "1. c4 e5");
    assert_eq!(automatic["results"][0]["effect"], "again_applied");
    let corrected = app
        .snapshot()
        .unwrap()
        .learning
        .into_iter()
        .find(|(_, state)| state.correction.is_some())
        .unwrap()
        .0;
    preview_and_save(&mut app, "1. d4 *", "Second", None);
    let ambiguous = date_only_import(&mut app, "ambiguous", "2024.10.06", "1. c4 e5");
    let result = &ambiguous["results"][0];
    assert_eq!(result["effect"], "choice_required");
    let accepted = app.choose_game_correction(serde_json::from_value(json!({
        "command_id":"choose-previously-corrected", "dataset_id":"opening-proof", "learner":"learner",
        "source":result["source"], "game_id":result["game_id"], "comparison_id":result["comparison_id"],
        "ply":1, "target_id":corrected
    })).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(accepted).unwrap()["results"][0]["effect"],
        "already_addressed"
    );
    let store = SqliteStore::open(&db).unwrap();
    let dataset = app.snapshot().unwrap().dataset;
    let before = store.game_relevance(&dataset, "").unwrap()[&corrected].clone();
    assert_eq!((before.exposure, before.misses), (1, 1));
    let learning = app.snapshot().unwrap().learning;
    let reprocessed = date_only_import(&mut app, "reprocessed", "2024.10.06", "1. c4 e5");
    assert_eq!(reprocessed["results"][0]["effect"], "already_addressed");
    assert_eq!(app.snapshot().unwrap().learning, learning);
    let after = store.game_relevance(&dataset, "").unwrap()[&corrected].clone();
    eprintln!(
        "already-addressed selected target {corrected:?}: before={before:?}, after={after:?}"
    );
    assert_eq!(
        after.misses, before.misses,
        "An accepted correction choice must retain its current-version miss attribution after reprocessing"
    );
}

#[test]
fn choosing_second_ambiguous_position_keeps_first_positions_miss() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("two-choices.sqlite");
    let day = Arc::new(AtomicI32::new(20_004));
    let mut app = open(&db, day, IntroductionLimits::default(), 0);
    preview_and_save(
        &mut app,
        "1. e4 e5 2. Nf3 Nc6 3. c4 Nf6 4. d4 d6 5. Nc3 *",
        "First",
        None,
    );
    preview_and_save(
        &mut app,
        "1. d4 e5 2. e4 Nc6 3. c4 Nf6 4. Nf3 d6 5. Be2 *",
        "Second",
        None,
    );
    let ambiguous = date_only_import(
        &mut app,
        "ambiguous-two",
        "2024.10.06",
        "1. c4 e5 2. Nf3 Nc6 3. d4 Nf6 4. e4 d6 5. g3",
    );
    let choices = ambiguous["results"][0]["choices"].as_array().unwrap();
    assert_eq!(choices.len(), 2, "{ambiguous}");
    assert!(
        choices
            .iter()
            .all(|choice| choice["candidates"].as_array().unwrap().len() == 2)
    );
    let first = TargetId::from(choices[0]["candidates"][0]["target_id"].as_str().unwrap());
    let second = TargetId::from(choices[1]["candidates"][0]["target_id"].as_str().unwrap());
    choose(&mut app, &ambiguous, &choices[0], "choose-first");
    let store = SqliteStore::open(&db).unwrap();
    let dataset = app.snapshot().unwrap().dataset;
    let before = store.game_relevance(&dataset, "").unwrap();
    assert_eq!(before[&first].misses, 1);
    choose(&mut app, &ambiguous, &choices[1], "choose-second");
    let after = store.game_relevance(&dataset, "").unwrap();
    assert_eq!(after[&second].misses, 1);
    eprintln!(
        "first position: before={:?}, after={:?}; second={:?}",
        before[&first], after[&first], after[&second]
    );
    assert_eq!(
        after[&first].misses, 1,
        "Choosing another position must preserve the first resolved position's miss attribution"
    );
}
