use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, BufRead, BufWriter, Write};
use tempo_chess::{ContentKind, TacticRecord, apply_setup_move, compile_tactics};

#[derive(Deserialize)]
struct Candidate {
    namespace: String,
    source_id: String,
    fen: String,
    moves: Vec<String>,
    setup: Option<String>,
}

fn validate(candidate: Candidate) -> Result<Value, tempo_chess::ChessError> {
    let solver_fen = match candidate.setup {
        Some(setup) => apply_setup_move(&candidate.fen, &setup)?,
        None => candidate.fen,
    };
    let record = TacticRecord {
        namespace: candidate.namespace,
        source_id: candidate.source_id,
        solver_fen,
        solution: candidate.moves,
        label: String::new(),
        provenance: Vec::new(),
    };
    let target = record.target_id();
    let catalog = compile_tactics(std::slice::from_ref(&record))?;
    let ContentKind::Tactic { practice_start } = &catalog.items[&target].definition.kind else {
        unreachable!("tactic compiler emits tactic definitions");
    };
    Ok(json!({
        "target": target.0,
        "practice_start": practice_start.0,
        "solver_fen": record.solver_fen,
        "solution": record.solution
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut output = BufWriter::new(io::stdout().lock());
    for line in io::stdin().lock().lines() {
        let candidate: Candidate = serde_json::from_str(&line?)?;
        let checked =
            validate(candidate).unwrap_or_else(|error| json!({ "error": error.to_string() }));
        writeln!(output, "{checked}")?;
    }
    output.flush()?;
    Ok(())
}
