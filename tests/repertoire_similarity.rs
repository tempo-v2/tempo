use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Stdio},
};

const START: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
const AFTER_E5: &str = "rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2";
const AFTER_C5: &str = "rnbqkbnr/pp1ppppp/8/2p5/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2";

fn query(fen: &str, rows: &[(&str, &str)]) -> (bool, Value) {
    let request = json!({
        "fen": fen,
        "repertoire": {
            "schema_version": 1,
            "basis": {"dataset":"test","repertoire_id":"test","color":"white","reply_revision":3},
            "replies": rows.iter().map(|(fen, movement)| json!({
                "fen":fen,"movement":movement,"answer_version":"test"
            })).collect::<Vec<_>>(),
        }
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_tempo"))
        .arg("repertoire-similarity")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    (
        output.status.success(),
        serde_json::from_slice(&output.stdout).unwrap(),
    )
}

#[test]
fn ranks_the_closest_example_once_per_legal_saved_move() {
    let clock_variant = AFTER_C5.replace("0 2", "1 4");
    let rows = [
        (START, "e2e4"),
        (START, "e2e4"),
        (AFTER_E5, "g1f3"),
        (&clock_variant, "d2d4"),
    ];
    let (success, result) = query(AFTER_C5, &rows);
    assert!(success);
    let moves = result["moves"].as_array().unwrap();
    assert_eq!(moves.len(), 2);
    assert_eq!(moves[0]["first_move"], "d2d4");
    assert_eq!(moves[0]["distance"], 0);
    assert_eq!(moves[1]["san"], "Nf3");
    assert_eq!(moves[1]["distance"], 12);
    let (_, result) = query(START, &[(AFTER_E5, "g1f3"), (AFTER_C5, "g1f3")]);
    assert_eq!(result["moves"].as_array().unwrap().len(), 1);
}

#[test]
fn distinguishes_empty_terminal_and_invalid() {
    assert_eq!(query(START, &[]).1["moves"], json!([]));
    let mate = "7k/6Q1/6K1/8/8/8/8/8 b - - 0 1";
    assert_eq!(
        query(mate, &[]).1["outcome"],
        json!({"kind":"checkmate","winner":"white"})
    );
    let (success, error) = query(START, &[(START, "e2e5")]);
    assert!(!success);
    assert_eq!(error["error"], "invalid_snapshot");
}

#[test]
fn compares_exact_special_move_identity() {
    let castle = "r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1";
    assert_eq!(
        query(castle, &[(castle, "e1g1")]).1["moves"][0]["first_move"],
        "e1g1"
    );
    let promotion = "7k/P7/8/8/8/8/8/7K w - - 0 1";
    assert_eq!(
        query(promotion, &[(promotion, "a7a8n")]).1["moves"][0]["first_move"],
        "a7a8n"
    );
}
