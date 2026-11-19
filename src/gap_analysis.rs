//! On-demand authoring analysis. Neither result can write content or influence Study.
use crate::{AppError, Tempo, app_error::domain, game_evidence::check_id, game_model::GapRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tempo_chess::{GapDraft, PositionKey, structural_neighbors};
use tempo_types::Revision;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapAnalysisRequest {
    pub request_id: String,
    pub gap: GapRequest,
}

pub(crate) struct GapAnalysisInput {
    pub draft: GapDraft,
    pub catalog: BTreeMap<PositionKey, Vec<String>>,
    pub revision: Revision,
}

#[derive(Deserialize, Serialize)]
struct EngineCandidate {
    uci: String,
    san: String,
    centipawns: Option<i32>,
    mate: Option<i32>,
}

#[derive(Deserialize, Serialize)]
struct EngineResult {
    engine: String,
    candidates: Vec<EngineCandidate>,
}

pub struct GapAnalysis {
    directory: PathBuf,
    reply: Arc<Mutex<Value>>,
}

impl GapAnalysis {
    pub fn new(database: &Path) -> Self {
        Self {
            directory: database.parent().unwrap_or(Path::new(".")).join("maia"),
            reply: Arc::new(Mutex::new(json!({"status":"idle"}))),
        }
    }

    pub fn status(&self) -> Value {
        self.reply.lock().expect("gap analysis reply").clone()
    }

    pub fn start(&self, app: &Tempo, request: GapAnalysisRequest) -> Result<Value, AppError> {
        check_id(&request.request_id)?;
        if self.status()["status"] == "running" {
            return Err(AppError::new(
                "analysis_busy",
                "A position analysis is already running.",
            ));
        }
        let input = app.gap_analysis_input(request.gap)?;
        let queued = json!({"status":"running","request_id":request.request_id,"content_revision":input.revision});
        let reply = Arc::clone(&self.reply);
        let id = request.request_id;
        let directory = self.directory.clone();
        *reply.lock().expect("gap analysis reply") = queued.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("tempo-gap-analysis".into())
            .spawn(move || {
                let result = analyze(id, input, &directory);
                *reply.lock().expect("gap analysis reply") = result;
            })
        {
            *self.reply.lock().expect("gap analysis reply") = json!({"status":"idle"});
            return Err(domain("analysis_unavailable", error));
        }
        Ok(queued)
    }
}

fn analyze(id: String, input: GapAnalysisInput, directory: &Path) -> Value {
    let neighbors =
        structural_neighbors(&input.draft.fen, input.catalog.keys(), 3).map(|positions| {
            positions
                .into_iter()
                .map(|position| {
                    json!({
                        "fen": format!("{} 0 1", position.0),
                        "repertoires": input.catalog[&position].iter().collect::<BTreeSet<_>>(),
                    })
                })
                .collect::<Vec<_>>()
        });
    let engine = stockfish(directory, &input.draft);
    json!({
        "status": "complete", "request_id": id, "content_revision": input.revision,
        "neighbors": neighbors.as_ref().ok(), "neighbors_error": neighbors.err().map(|error| error.to_string()),
        "stockfish": engine.as_ref().ok(), "stockfish_error": engine.err().map(|error| error.message),
    })
}

fn stockfish(directory: &Path, draft: &GapDraft) -> Result<EngineResult, AppError> {
    let binary = [
        "/opt/homebrew/bin/stockfish",
        "/usr/local/bin/stockfish",
        "/usr/bin/stockfish",
    ]
    .into_iter()
    .find(|name| Path::new(name).is_file())
    .ok_or_else(|| {
        AppError::new(
            "engine_unavailable",
            "Stockfish was not found on this computer.",
        )
    })?;
    if draft.legal_moves.is_empty() {
        return Ok(EngineResult {
            engine: "Stockfish".into(),
            candidates: vec![],
        });
    }
    let mut command = Command::new(directory.join("venv/bin/python"));
    command
        .args([
            "-I",
            "-c",
            include_str!("../tools/stockfish/candidates.py"),
            binary,
            &draft.fen,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut process = start_engine_wrapper(&mut command)?;
    wait_for_engine(&mut process, Duration::from_secs(8))?;
    let output = process
        .wait_with_output()
        .map_err(|error| domain("engine_unavailable", error))?;
    if !output.status.success() {
        return Err(AppError::new(
            "engine_unavailable",
            "Stockfish could not evaluate the position.",
        ));
    }
    let reply: EngineResult = serde_json::from_slice(&output.stdout)
        .map_err(|error| domain("invalid_engine_reply", error))?;
    let unique: BTreeSet<_> = reply
        .candidates
        .iter()
        .map(|move_result| &move_result.uci)
        .collect();
    if reply.candidates.is_empty()
        || reply.candidates.len() > 3
        || reply.candidates.len() != unique.len()
        || !unique
            .into_iter()
            .all(|uci| draft.legal_moves.contains(uci))
    {
        return Err(AppError::new(
            "invalid_engine_reply",
            "Stockfish returned an invalid candidate list.",
        ));
    }
    Ok(reply)
}

fn start_engine_wrapper(command: &mut Command) -> Result<Child, AppError> {
    // Stockfish and the Python UCI wrapper share the owned process group.
    #[cfg(unix)]
    command.process_group(0);
    command
        .spawn()
        .map_err(|error| domain("engine_unavailable", error))
}

fn wait_for_engine(process: &mut Child, timeout: Duration) -> Result<(), AppError> {
    let deadline = Instant::now() + timeout;
    loop {
        match process.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            outcome => {
                #[cfg(unix)]
                let _ = Command::new("/bin/kill")
                    .args(["-s", "KILL", "--", &format!("-{}", process.id())])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = process.kill();
                let _ = process.wait();
                return match outcome {
                    Err(error) => Err(domain("engine_unavailable", error)),
                    _ => Err(AppError::new(
                        "engine_timeout",
                        "Stockfish did not finish the bounded search.",
                    )),
                };
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    #[test]
    fn timeout_kills_the_wrapper_and_its_engine() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 60 & echo $!; wait"])
            .stdout(Stdio::piped());
        let mut wrapper = start_engine_wrapper(&mut command).unwrap();
        let mut engine_id = String::new();
        BufReader::new(wrapper.stdout.take().unwrap())
            .read_line(&mut engine_id)
            .unwrap();
        let engine_id = engine_id.trim();
        assert!(!engine_id.is_empty());
        assert_eq!(
            wait_for_engine(&mut wrapper, Duration::ZERO)
                .unwrap_err()
                .code,
            "engine_timeout"
        );
        assert!(wrapper.try_wait().unwrap().is_some());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let child = Command::new("/bin/ps")
                .args(["-o", "stat=", "-p", engine_id])
                .output()
                .unwrap();
            if !child.status.success()
                || String::from_utf8_lossy(&child.stdout)
                    .trim()
                    .starts_with('Z')
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "engine process {engine_id} survived"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
