//! On-demand authoring analysis. Neither result can write content or influence Study.
use crate::{
    AppError, Tempo,
    app_error::domain,
    game_evidence::check_id,
    game_model::GapRequest,
    gap_recommendation::{SimilarMoves, recommend},
};
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
use tempo_chess::{GapDraft, PositionKey, SavedRepertoireResponse, nearby_repertoire_positions};
use tempo_types::Revision;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapAnalysisRequest {
    pub request_id: String,
    pub gap: GapRequest,
}

pub(crate) struct GapAnalysisInput {
    pub draft: GapDraft,
    pub catalog: BTreeMap<PositionKey, Vec<SavedRepertoireResponse>>,
    pub repertoire_id: String,
    pub repertoire_names: BTreeMap<String, String>,
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
    #[serde(skip_serializing)]
    comparison: Option<Vec<EngineCandidate>>,
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
                let result = analyze(id, input, &directory, |progress| {
                    *reply.lock().expect("gap analysis reply") = progress;
                });
                *reply.lock().expect("gap analysis reply") = result;
            })
        {
            *self.reply.lock().expect("gap analysis reply") = json!({"status":"idle"});
            return Err(domain("analysis_unavailable", error));
        }
        Ok(queued)
    }
}

const NEARBY_PER_GROUP: usize = 8;
const MAX_COMPARISON_REPLIES: usize = 16;

fn analyze(
    id: String,
    input: GapAnalysisInput,
    directory: &Path,
    publish: impl FnOnce(Value),
) -> Value {
    let nearby = nearby_repertoire_positions(
        &input.draft.fen,
        &input.repertoire_id,
        &input.catalog,
        NEARBY_PER_GROUP,
    );
    let similar = nearby.as_ref().map(SimilarMoves::rank).unwrap_or_default();
    // Older open pages can continue reading the original three board examples.
    let neighbors = nearby.as_ref().ok().map(|groups| {
        let mut positions = BTreeMap::<_, BTreeSet<_>>::new();
        for position in groups.current.iter().chain(&groups.other) {
            positions
                .entry((position.distance, &position.fen))
                .or_default()
                .extend(
                    position
                        .responses
                        .iter()
                        .filter_map(|saved| input.repertoire_names.get(&saved.repertoire_id)),
                );
        }
        positions
            .into_iter()
            .take(3)
            .map(|((_, fen), names)| json!({"fen": fen, "repertoires": names}))
            .collect::<Vec<_>>()
    });
    let mut progress = json!({
        "status": "running", "request_id": id, "content_revision": input.revision,
        "nearby": nearby.as_ref().ok(), "neighbors": neighbors, "neighbors_error": nearby.as_ref().err().map(ToString::to_string),
        "similar_responses": similar, "repertoire_names": input.repertoire_names,
    });
    publish(progress.clone());

    let shortlist = similar
        .ordered()
        .take(MAX_COMPARISON_REPLIES)
        .map(|(_, mv)| mv.uci)
        .collect::<Vec<_>>();
    let mut engine = stockfish(directory, &input.draft, &shortlist);
    let recommendation = engine.as_ref().ok().and_then(|result| {
        let best = result
            .comparison
            .as_ref()
            .unwrap_or(&result.candidates)
            .first()?;
        let scores = result
            .comparison
            .iter()
            .flatten()
            .map(|mv| (mv.uci.as_str(), mv.centipawns));
        recommend(&similar, (&best.uci, best.centipawns), scores)
    });
    progress["recommendation"] = json!(recommendation);
    if let Ok(result) = &mut engine
        && let Some(comparison) = result.comparison.take()
    {
        result.candidates = comparison.into_iter().take(3).collect();
    }
    progress["stockfish"] = json!(engine.as_ref().ok());
    progress["stockfish_error"] = json!(engine.err().map(|error| error.message));
    progress["status"] = json!("complete");
    progress
}

fn stockfish(
    directory: &Path,
    draft: &GapDraft,
    shortlist: &[&str],
) -> Result<EngineResult, AppError> {
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
            comparison: None,
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
        .args(shortlist)
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
    let mut reply: EngineResult = serde_json::from_slice(&output.stdout)
        .map_err(|error| domain("invalid_engine_reply", error))?;
    let unique: BTreeSet<_> = reply
        .candidates
        .iter()
        .map(|move_result| move_result.uci.as_str())
        .collect();
    if reply.candidates.is_empty()
        || reply.candidates.len() > 3
        || reply.candidates.len() != unique.len()
        || reply
            .candidates
            .iter()
            .any(|mv| mv.centipawns.is_some() == mv.mate.is_some())
        || !reply
            .candidates
            .iter()
            .all(|mv| draft.legal_moves.contains(&mv.uci))
    {
        return Err(AppError::new(
            "invalid_engine_reply",
            "Stockfish returned an invalid candidate list.",
        ));
    }
    let expected = unique
        .into_iter()
        .chain(shortlist.iter().copied())
        .collect::<BTreeSet<_>>();
    if reply.comparison.as_ref().is_some_and(|moves| {
        moves.len() != expected.len()
            || moves
                .iter()
                .map(|mv| mv.uci.as_str())
                .collect::<BTreeSet<_>>()
                != expected
            || moves
                .iter()
                .any(|mv| mv.centipawns.is_some() == mv.mate.is_some())
    }) {
        reply.comparison = None;
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
