use serde_json::{Value, json};
use std::path::Path;
use tempo::game_backfill::BackfillCoordinator;
use tempo::{DailyComponents, FixedClock, IntroductionLimits, Tempo};
use tempo_chess::{Catalog, LegalSequenceGrader};
use tempo_learning::{DailyLearning, LearningPolicy};
use tempo_store::{SqliteStore, Store};
use tempo_types::{DatasetId, StudyDay, StudyTime};

const DAY: u64 = 86_400_000;
const NOW: u64 = 20_004 * DAY;

fn app(path: &Path) -> Tempo {
    let learning = DailyLearning::default();
    let store = SqliteStore::create(
        path,
        "audit".into(),
        Catalog::default(),
        learning.settings(),
    )
    .unwrap();
    Tempo::daily(
        Box::new(store),
        DailyComponents {
            clock: Box::new(FixedClock(StudyTime {
                day: StudyDay(20_004),
                unix_ms: NOW,
            })),
            grader: Box::new(LegalSequenceGrader),
            learning: Box::new(learning),
            ids: Box::new(tempo::CounterIds::default()),
        },
        IntroductionLimits::default(),
        [42; 32],
        0,
    )
    .unwrap()
}

fn save_book(app: &mut Tempo) {
    let mut request = json!({"command":"preview","dataset_id":"audit","name":"Book",
        "learner_color":"white","pgn":"1. e4 e5 2. Nf3 *"});
    let preview = app.handle_repertoire(serde_json::from_value(request.clone()).unwrap());
    assert_eq!(preview["status"], "preview", "{preview}");
    request["command"] = json!("save");
    request["token"] = preview["token"].clone();
    let saved = app.handle_repertoire(serde_json::from_value(request).unwrap());
    assert_eq!(saved["status"], "repertoires", "{saved}");
}

fn import_correct_game(app: &mut Tempo, learner: &str) {
    let report = app.import_game_pgn(serde_json::from_value(json!({
        "command_id": format!("game-{learner}"), "dataset_id":"audit", "learner":learner,
        "pgn":format!("[White \"{learner}\"]\n[Black \"opponent\"]\n[Date \"2024.10.06\"]\n\n1. e4 c5 1-0")
    })).unwrap()).unwrap();
    assert_eq!(report.new_games, 1);
}

#[test]
fn narrower_backfill_preserves_recognized_account_evidence_before_fetching() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("audit.sqlite");
    let mut app = app(&path);
    save_book(&mut app);
    import_correct_game(&mut app, "lichess-user");
    import_correct_game(&mut app, "chess-user");
    import_correct_game(&mut app, "unrecognized");
    let dataset = DatasetId::from("audit");
    let mut store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.opponent_gaps(&dataset).unwrap()[0].games, 3);
    let completed = json!({"id":"completed-both", "request":{
        "command_id":"completed-both", "dataset_id":"audit", "from_ms":NOW-3*DAY,
        "to_ms":NOW-DAY, "lichess":"lichess-user", "chess_com":"chess-user"
    }, "resets":{}, "windows":[], "retries":[]});
    store
        .save_game_backfill_state(
            &dataset,
            "completed-both",
            &completed.to_string(),
            Some(&[
                ("lichess".into(), "lichess-user".into()),
                ("chess_com".into(), "chess-user".into()),
            ]),
        )
        .unwrap();
    let before = store.game_relevance(&dataset, "").unwrap();
    let target = before
        .iter()
        .find(|(_, value)| value.exposure == 2)
        .unwrap()
        .0
        .clone();
    assert_eq!(store.opponent_gaps(&dataset).unwrap()[0].games, 2);
    let mut coordinator = BackfillCoordinator::start(&app).unwrap();
    coordinator
        .begin(
            &mut app,
            serde_json::from_value(json!({
                "command_id":"lichess-only", "dataset_id":"audit", "from_ms":NOW-3*DAY,
                "to_ms":NOW-DAY, "lichess":"lichess-user", "chess_com":null
            }))
            .unwrap(),
        )
        .unwrap();
    let after = store.game_relevance(&dataset, "").unwrap();
    println!(
        "Narrower job began, no poll or fetch: exposure {} -> {}, gap games 2 -> {}",
        before[&target].exposure,
        after[&target].exposure,
        store.opponent_gaps(&dataset).unwrap()[0].games
    );
    assert_eq!(after[&target].exposure, 2);
    assert_eq!(store.opponent_gaps(&dataset).unwrap()[0].games, 2);
    let state = store
        .game_backfill_state(&dataset, Some("lichess-only"))
        .unwrap()
        .unwrap();
    let state: Value = serde_json::from_str(&state).unwrap();
    let windows = state["windows"].as_array().unwrap();
    assert!(windows.iter().any(|window| window["source"] == "lichess"));
    assert!(windows.iter().any(|window| window["source"] == "pgn"));
    assert!(
        windows
            .iter()
            .all(|window| window["learner"] == "lichess-user"
                && matches!(window["source"].as_str(), Some("lichess" | "pgn")))
    );
}
