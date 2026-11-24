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
use tempo_types::{ContentVersion, StudyDay, StudyTime};

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
    let (app, repair) = startup(1);
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
fn legacy_choice_attribution_does_not_move_to_a_new_answer_version() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("versioned-legacy-choice.sqlite");
    std::fs::write(&db, include_bytes!("fixtures/schema9-choice.sqlite")).unwrap();
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/schema9-choice.json")).unwrap();
    let choice: tempo::game_evidence::ChooseGameCorrection =
        serde_json::from_value(fixture["command"].clone()).unwrap();
    let mut app = open(&db, Arc::new(AtomicI32::new(20_004)), IntroductionLimits::default(), 1);
    let original = app.snapshot().unwrap();
    let original_item = &original.catalog.items[&choice.target_id];
    let tempo_chess::ContentKind::Repertoire { repertoire, .. } = &original_item.definition.kind else {
        panic!("historical choice must reference a repertoire");
    };
    preview_and_save(&mut app, "1. d3 *", &original_item.label, Some(repertoire));

    let edited = app.snapshot().unwrap();
    assert_ne!(edited.catalog.items[&choice.target_id].version, original_item.version);
    date_only_import(&mut app, "reimport-after-edit", "2024.10.06", "1. c4 e5");
    assert_eq!(app.snapshot().unwrap().learning, edited.learning);

    let store = SqliteStore::open(&db).unwrap();
    let choices = store.game_choices(&choice.dataset_id, &choice.source, &choice.game_id).unwrap();
    assert_eq!(
        choices[fixture["position"].as_str().unwrap()],
        Some(ChoiceAttribution {
            target: choice.target_id,
            answer_version: original_item.version,
        })
    );
}
