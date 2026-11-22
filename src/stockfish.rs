//! Bounded engine comparisons for a validated position and candidate moves.
use crate::{AppError, app_error::domain};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::sync::Mutex;
use std::{
    collections::BTreeSet,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
static ENGINE_SLOT: Mutex<()> = Mutex::new(());

#[derive(Deserialize, Serialize)]
pub(crate) struct EngineCandidate {
    pub(crate) uci: String,
    san: String,
    pub(crate) centipawns: Option<i32>,
    mate: Option<i32>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct EngineResult {
    pub(crate) engine: String,
    pub(crate) candidates: Vec<EngineCandidate>,
    #[serde(skip_serializing)]
    pub(crate) comparison: Option<Vec<EngineCandidate>>,
}

pub(crate) fn stockfish_until(
    directory: &Path,
    fen: &str,
    legal_moves: &[String],
    shortlist: &[&str],
    candidate_count: usize,
    deadline: Option<Instant>,
) -> Result<EngineResult, AppError> {
    let remaining = |limit: Duration| {
        deadline.map_or(limit, |end| {
            limit.min(end.saturating_duration_since(Instant::now()))
        })
    };
    let _diagnostic = crate::diagnostics::Span::new("engine.evaluate");
    let slot_wait = if deadline.is_some() { 10 } else { 2 };
    let slot_deadline = Instant::now() + remaining(Duration::from_secs(slot_wait));
    let _slot = loop {
        match ENGINE_SLOT.try_lock() {
            Ok(slot) => break slot,
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(AppError::new(
                    "engine_unavailable",
                    "Engine slot unavailable.",
                ));
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        if Instant::now() >= slot_deadline {
            return Err(AppError::new(
                "engine_busy",
                "Stockfish is working on another request.",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
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
    if legal_moves.is_empty() {
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
            fen,
        ])
        .arg(candidate_count.to_string())
        .args(shortlist)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let timeout = remaining(Duration::from_secs(8));
    if timeout.is_zero() {
        return Err(AppError::new(
            "engine_timeout",
            "The line preview reached its time limit.",
        ));
    }
    let mut process = start_engine_wrapper(&mut command)?;
    wait_for_engine(&mut process, timeout)?;
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
        || reply.candidates.len() > candidate_count
        || reply.candidates.len() != unique.len()
        || reply
            .candidates
            .iter()
            .any(|mv| mv.centipawns.is_some() == mv.mate.is_some())
        || !reply
            .candidates
            .iter()
            .all(|mv| legal_moves.contains(&mv.uci))
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
    let _diagnostic = crate::diagnostics::Span::new("engine.spawn");
    // Stockfish and the Python UCI wrapper share the owned process group.
    #[cfg(unix)]
    command.process_group(0);
    let result = command
        .spawn()
        .map_err(|error| domain("engine_unavailable", error));
    if let Ok(process) = &result {
        crate::diagnostics::emit(
            "process.spawned",
            serde_json::json!({
                "operation":"engine.spawn", "child_pid":process.id()
            }),
        );
    }
    result
}

fn wait_for_engine(process: &mut Child, timeout: Duration) -> Result<(), AppError> {
    let mut trace = crate::diagnostics::Span::new("engine.wait");
    trace.field("timeout_ms", timeout.as_millis());
    trace.field("child_pid", process.id());
    let deadline = Instant::now() + timeout;
    loop {
        match process.try_wait() {
            Ok(Some(_)) => {
                trace.set_outcome("child_exited");
                return Ok(());
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            outcome => {
                trace.set_outcome(if outcome.is_err() {
                    "io_error"
                } else {
                    "timed_out"
                });
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
