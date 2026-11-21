//! Process ownership for the Chess tactical evaluator port.
//!
//! One Python/Stockfish pair is reused by the detector worker. The adapter only
//! returns bounded engine evidence; Chess owns every tactical classification.
use crate::AppError;
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Mutex, TryLockError,
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    time::{Duration, Instant},
};
use tempo_chess::DetectionStop;
use tempo_chess::{
    DetectionControl, EvaluationFailure, EvaluationProvenance, RootEvaluation, RootScore,
    TacticalEvaluationRequest, TacticalEvaluator,
};

const SCRIPT: &str = include_str!("../tools/stockfish/hanging.py");
const START_TIMEOUT: Duration = Duration::from_secs(8);
const POSITION_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_MESSAGE_BYTES: usize = 1_048_576;

#[derive(Clone)]
struct EngineConfiguration {
    python: PathBuf,
    binary: PathBuf,
    nodes: u64,
    min_depth: u32,
}

#[derive(Deserialize)]
struct Ready {
    protocol: u8,
    status: String,
    engine: String,
    python_chess_version: String,
}

#[derive(Deserialize)]
struct EngineReply {
    request_id: u64,
    status: String,
    depth: Option<u32>,
    nodes: Option<u64>,
    roots: Option<Vec<RootScore>>,
}

pub(crate) struct StockfishEvaluator {
    configuration: EngineConfiguration,
    provenance: EvaluationProvenance,
    ready_identity: (String, String),
    binary_digest: blake3::Hash,
    worker: Mutex<Option<Worker>>,
}

impl StockfishEvaluator {
    pub(crate) fn discover(database: &Path) -> Result<Self, AppError> {
        let python = std::env::var_os("TEMPO_TACTICAL_PYTHON")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                database
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join("maia/venv/bin/python")
            });
        let binary = std::env::var_os("TEMPO_STOCKFISH")
            .map(PathBuf::from)
            .or_else(|| {
                [
                    "/opt/homebrew/bin/stockfish",
                    "/usr/local/bin/stockfish",
                    "/usr/bin/stockfish",
                    "/usr/games/stockfish",
                ]
                .into_iter()
                .map(PathBuf::from)
                .find(|path| path.is_file())
            })
            .ok_or_else(|| {
                AppError::new(
                    "engine_unavailable",
                    "Stockfish is unavailable for game tactics.",
                )
            })?;
        if !python.is_file() || !binary.is_file() {
            return Err(AppError::new(
                "engine_unavailable",
                "The tactical Python environment or Stockfish executable is unavailable.",
            ));
        }
        let configuration = EngineConfiguration {
            python,
            binary,
            nodes: 2_000_000,
            min_depth: 12,
        };
        Self::start(configuration)
    }

    fn start(configuration: EngineConfiguration) -> Result<Self, AppError> {
        let registered_digest = binary_digest(&configuration.binary, None).map_err(|_| {
            AppError::new(
                "engine_unavailable",
                "Stockfish identity could not be read.",
            )
        })?;
        let (worker, ready) = Worker::start(&configuration, None).map_err(|_| {
            AppError::new(
                "engine_unavailable",
                "Stockfish could not start the tactical evaluator.",
            )
        })?;
        if binary_digest(&configuration.binary, None).ok() != Some(registered_digest) {
            return Err(AppError::new(
                "engine_unavailable",
                "Stockfish changed during evaluator startup.",
            ));
        }
        let engine_version = format!("{} · blake3:{}", ready.engine, registered_digest.to_hex());
        let settings = json!([
            SCRIPT,
            ready.python_chess_version,
            configuration.nodes,
            configuration.min_depth,
            64,
            1
        ]);
        let provenance = EvaluationProvenance {
            engine: "Stockfish".into(),
            engine_version,
            configuration_fingerprint: blake3::hash(settings.to_string().as_bytes())
                .to_hex()
                .to_string(),
        };
        Ok(Self {
            configuration,
            provenance,
            ready_identity: (ready.engine, ready.python_chess_version),
            binary_digest: registered_digest,
            worker: Mutex::new(Some(worker)),
        })
    }
}

struct YieldToForeground<'a>(&'a dyn DetectionControl);

impl DetectionControl for YieldToForeground<'_> {
    fn deeper_search(&self) -> bool {
        self.0.deeper_search()
    }

    fn check(&self) -> Result<(), DetectionStop> {
        if crate::stockfish::foreground_waiting() {
            return Err(DetectionStop::Cancelled);
        }
        self.0.check()
    }
}

impl TacticalEvaluator for StockfishEvaluator {
    fn descriptor(&self) -> EvaluationProvenance {
        self.provenance.clone()
    }

    fn evaluate(
        &self,
        request: &TacticalEvaluationRequest,
        control: &dyn DetectionControl,
    ) -> Result<RootEvaluation, EvaluationFailure> {
        let control = &YieldToForeground(control);
        let _engine = loop {
            control.check().map_err(EvaluationFailure::Stopped)?;
            if let Some(slot) = crate::stockfish::background_slot() {
                break slot;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut slot = loop {
            control.check().map_err(EvaluationFailure::Stopped)?;
            match self.worker.try_lock() {
                Ok(slot) => break slot,
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(10)),
                Err(TryLockError::Poisoned(_)) => return Err(EvaluationFailure::Failed),
            }
        };
        if slot.is_none() {
            if binary_digest(&self.configuration.binary, Some(control))? != self.binary_digest {
                return Err(EvaluationFailure::Unavailable);
            }
            let (worker, ready) = Worker::start(&self.configuration, Some(control))?;
            if (ready.engine, ready.python_chess_version) != self.ready_identity {
                return Err(EvaluationFailure::Unavailable);
            }
            if binary_digest(&self.configuration.binary, Some(control))? != self.binary_digest {
                return Err(EvaluationFailure::Unavailable);
            }
            *slot = Some(worker);
        }
        let result = slot
            .as_mut()
            .expect("worker initialized")
            .evaluate(request, control);
        match result {
            Ok(reply) => Ok(RootEvaluation {
                provenance: self.provenance.clone(),
                depth: reply.depth.ok_or(EvaluationFailure::Incomplete)?,
                nodes: reply.nodes.ok_or(EvaluationFailure::Incomplete)?,
                roots: reply.roots.ok_or(EvaluationFailure::Incomplete)?,
            }),
            Err(EvaluationFailure::Incomplete) => Err(EvaluationFailure::Incomplete),
            Err(error) => {
                // No later response can be mistaken for a new request's result.
                *slot = None;
                Err(error)
            }
        }
    }
}

struct Worker {
    _process: OwnedProcess,
    input: SyncSender<Vec<u8>>,
    output: Receiver<Result<Value, ()>>,
    sequence: u64,
    nodes: u64,
    min_depth: u32,
}

impl Worker {
    fn start(
        configuration: &EngineConfiguration,
        control: Option<&dyn DetectionControl>,
    ) -> Result<(Self, Ready), EvaluationFailure> {
        Self::start_script(configuration, control, SCRIPT)
    }

    fn start_script(
        configuration: &EngineConfiguration,
        control: Option<&dyn DetectionControl>,
        script: &str,
    ) -> Result<(Self, Ready), EvaluationFailure> {
        check_control(control)?;
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("/usr/bin/nice");
            command.args(["-n", "10"]).arg(&configuration.python);
            command
        };
        #[cfg(not(unix))]
        let mut command = Command::new(&configuration.python);
        command
            .args(["-I", "-u", "-c", script])
            .arg(&configuration.binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        command.process_group(0);
        // Ownership starts immediately: even failed pipe/thread/handshake setup
        // must kill and reap the process and its Stockfish child.
        let mut process = OwnedProcess(
            command
                .spawn()
                .map_err(|_| EvaluationFailure::Unavailable)?,
        );
        let mut input = process
            .0
            .stdin
            .take()
            .ok_or(EvaluationFailure::Unavailable)?;
        let output = process
            .0
            .stdout
            .take()
            .ok_or(EvaluationFailure::Unavailable)?;
        let (sender, receiver) = mpsc::sync_channel(2);
        let write_error = sender.clone();
        std::thread::Builder::new()
            .name("tempo-tactical-output".into())
            .spawn(move || {
                let mut reader = BufReader::new(output);
                loop {
                    let mut line = Vec::new();
                    let result = match (&mut reader)
                        .take((MAX_MESSAGE_BYTES + 1) as u64)
                        .read_until(b'\n', &mut line)
                    {
                        Ok(0) => break,
                        Ok(_) if line.len() <= MAX_MESSAGE_BYTES && line.ends_with(b"\n") => {
                            serde_json::from_slice(&line).map_err(|_| ())
                        }
                        _ => Err(()),
                    };
                    let invalid = result.is_err();
                    if sender.send(result).is_err() || invalid {
                        break;
                    }
                }
            })
            .map_err(|_| EvaluationFailure::Unavailable)?;
        let (send_input, receive_input) = mpsc::sync_channel::<Vec<u8>>(1);
        // A blocked pipe write must not make cancellation wait for the engine.
        // Killing the owned process closes the pipe and releases this writer.
        std::thread::Builder::new()
            .name("tempo-tactical-input".into())
            .spawn(move || {
                while let Ok(payload) = receive_input.recv() {
                    if input
                        .write_all(&payload)
                        .and_then(|()| input.flush())
                        .is_err()
                    {
                        let _ = write_error.send(Err(()));
                        break;
                    }
                }
            })
            .map_err(|_| EvaluationFailure::Unavailable)?;
        let mut worker = Self {
            _process: process,
            input: send_input,
            output: receiver,
            sequence: 0,
            nodes: configuration.nodes,
            min_depth: configuration.min_depth,
        };
        let hello = worker.receive(control, Instant::now() + START_TIMEOUT)?;
        let ready: Ready =
            serde_json::from_value(hello).map_err(|_| EvaluationFailure::Unavailable)?;
        if ready.protocol != 1
            || ready.status != "ready"
            || ready.engine.is_empty()
            || ready.python_chess_version.is_empty()
        {
            return Err(EvaluationFailure::Unavailable);
        }
        Ok((worker, ready))
    }

    fn receive(
        &mut self,
        control: Option<&dyn DetectionControl>,
        deadline: Instant,
    ) -> Result<Value, EvaluationFailure> {
        loop {
            check_control(control)?;
            if Instant::now() >= deadline {
                return Err(EvaluationFailure::Unavailable);
            }
            match self.output.recv_timeout(Duration::from_millis(20)) {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => {
                    return Err(EvaluationFailure::Failed);
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn evaluate(
        &mut self,
        request: &TacticalEvaluationRequest,
        control: &dyn DetectionControl,
    ) -> Result<EngineReply, EvaluationFailure> {
        control.check().map_err(EvaluationFailure::Stopped)?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(EvaluationFailure::Failed)?;
        let mut payload = serde_json::to_value(request).map_err(|_| EvaluationFailure::Failed)?;
        payload["request_id"] = json!(self.sequence);
        payload["nodes"] = json!(if control.deeper_search() {
            8_000_000
        } else {
            self.nodes
        });
        payload["min_depth"] = json!(self.min_depth);
        let mut payload = serde_json::to_vec(&payload).map_err(|_| EvaluationFailure::Failed)?;
        payload.push(b'\n');
        if payload.len() > MAX_MESSAGE_BYTES {
            return Err(EvaluationFailure::Failed);
        }
        self.input
            .try_send(payload)
            .map_err(|_| EvaluationFailure::Unavailable)?;
        let value = self.receive(
            Some(control),
            Instant::now()
                + if control.deeper_search() {
                    Duration::from_secs(110)
                } else {
                    POSITION_TIMEOUT
                },
        )?;
        let reply: EngineReply =
            serde_json::from_value(value).map_err(|_| EvaluationFailure::Failed)?;
        if reply.request_id != self.sequence {
            return Err(EvaluationFailure::Failed);
        }
        match reply.status.as_str() {
            "complete" => Ok(reply),
            "incomplete" => Err(EvaluationFailure::Incomplete),
            "unavailable" => Err(EvaluationFailure::Unavailable),
            _ => Err(EvaluationFailure::Failed),
        }
    }
}

fn check_control(control: Option<&dyn DetectionControl>) -> Result<(), EvaluationFailure> {
    match control {
        Some(control) => control.check().map_err(EvaluationFailure::Stopped),
        None => Ok(()),
    }
}

fn binary_digest(
    path: &Path,
    control: Option<&dyn DetectionControl>,
) -> Result<blake3::Hash, EvaluationFailure> {
    check_control(control)?;
    let mut binary = File::open(path).map_err(|_| EvaluationFailure::Unavailable)?;
    if !binary
        .metadata()
        .map_err(|_| EvaluationFailure::Unavailable)?
        .is_file()
    {
        return Err(EvaluationFailure::Unavailable);
    }
    let mut hash = blake3::Hasher::new();
    let mut buffer = [0_u8; 65_536];
    loop {
        check_control(control)?;
        let count = binary
            .read(&mut buffer)
            .map_err(|_| EvaluationFailure::Unavailable)?;
        if count == 0 {
            return Ok(hash.finalize());
        }
        hash.update(&buffer[..count]);
    }
}

/// Child alone does not terminate its process on Drop.
struct OwnedProcess(Child);

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = Command::new("/bin/kill")
            .args(["-s", "KILL", "--", &format!("-{}", self.0.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The generator searches the accepted root separately. This does not change
/// detector identity or invalidate the retained archive of game observations.
#[derive(Clone)]
pub(crate) struct PayoffEngine(EngineConfiguration);

impl StockfishEvaluator {
    pub(crate) fn payoff_engine(&self) -> PayoffEngine {
        PayoffEngine(self.configuration.clone())
    }
}

impl PayoffEngine {
    pub(crate) fn variation(
        &self,
        fen: &str,
        root: &tempo_chess::ChessAnswer,
        control: &dyn DetectionControl,
    ) -> Result<Vec<String>, EvaluationFailure> {
        let control = YieldToForeground(control);
        let deadline = Instant::now() + POSITION_TIMEOUT;
        let _slot = loop {
            control.check().map_err(EvaluationFailure::Stopped)?;
            if let Some(slot) = crate::stockfish::background_slot() {
                break slot;
            }
            if Instant::now() >= deadline {
                return Err(EvaluationFailure::Incomplete);
            }
            std::thread::sleep(Duration::from_millis(15));
        };
        let mut command = Command::new(&self.0.python);
        command
            .args(["-I", "-c", include_str!("../tools/stockfish/payoff.py")])
            .arg(&self.0.binary)
            .args([fen, &root.uci])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        command.process_group(0);
        let mut child = OwnedProcess(
            command
                .spawn()
                .map_err(|_| EvaluationFailure::Unavailable)?,
        );
        loop {
            control.check().map_err(EvaluationFailure::Stopped)?;
            if let Some(status) = child.0.try_wait().map_err(|_| EvaluationFailure::Failed)? {
                if !status.success() {
                    return Err(EvaluationFailure::Failed);
                }
                let stdout = child.0.stdout.take().ok_or(EvaluationFailure::Failed)?;
                return serde_json::from_reader(stdout.take(1024))
                    .map_err(|_| EvaluationFailure::Failed);
            }
            if Instant::now() >= deadline {
                return Err(EvaluationFailure::Incomplete);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempo_chess::{DetectionStop, EvaluationScore, legal_move_labels};

    struct Running;
    impl DetectionControl for Running {
        fn check(&self) -> Result<(), DetectionStop> {
            Ok(())
        }
    }

    struct CancelAt(Instant);
    impl DetectionControl for CancelAt {
        fn check(&self) -> Result<(), DetectionStop> {
            if Instant::now() >= self.0 {
                Err(DetectionStop::Cancelled)
            } else {
                Ok(())
            }
        }
    }

    fn python() -> PathBuf {
        std::env::var_os("TEMPO_TACTICAL_PYTHON")
            .map(PathBuf::from)
            .or_else(|| {
                ["/usr/bin/python3", "/usr/local/bin/python3"]
                    .into_iter()
                    .map(PathBuf::from)
                    .find(|path| path.is_file())
            })
            .expect("Python is required for the tactical process contract tests")
    }

    fn configuration(binary: &Path) -> EngineConfiguration {
        EngineConfiguration {
            python: python(),
            binary: binary.to_path_buf(),
            nodes: 500_000,
            min_depth: 12,
        }
    }

    fn request() -> TacticalEvaluationRequest {
        let fen = "7k/n7/8/8/8/8/8/R6K w - - 0 1";
        TacticalEvaluationRequest {
            start_fen: fen.into(),
            moves: Vec::new(),
            fen: fen.into(),
            legal_moves: legal_move_labels(fen).unwrap().into_keys().collect(),
        }
    }

    const HELLO: &str = r#"
import json, sys, time
print(json.dumps({'protocol':1,'status':'ready','engine':'Stockfish fixture','python_chess_version':'fixture'}), flush=True)
"#;

    fn fixture(configuration: EngineConfiguration, worker: Option<Worker>) -> StockfishEvaluator {
        let digest = binary_digest(&configuration.binary, None).unwrap();
        StockfishEvaluator {
            configuration,
            provenance: EvaluationProvenance {
                engine: "fixture".into(),
                engine_version: "fixture".into(),
                configuration_fingerprint: "fixture".into(),
            },
            ready_identity: ("Stockfish fixture".into(), "fixture".into()),
            binary_digest: digest,
            worker: Mutex::new(worker),
        }
    }

    #[test]
    fn sends_explicit_budget_and_rejects_a_reply_for_another_request() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("identity");
        std::fs::write(&binary, "fixture").unwrap();
        let config = configuration(&binary);
        let script = format!(
            "{HELLO}\n{}",
            r#"
for raw in sys.stdin:
    request=json.loads(raw)
    assert request['nodes']==500000 and request['min_depth']==12
    print(json.dumps({'request_id':request['request_id']+1,'status':'complete','depth':12,'nodes':500000,'roots':[]}), flush=True)
"#
        );
        let (worker, _) = Worker::start_script(&config, None, &script).unwrap();
        let evaluator = fixture(config, Some(worker));
        assert_eq!(
            evaluator.evaluate(&request(), &Running),
            Err(EvaluationFailure::Failed)
        );
        assert!(evaluator.worker.lock().unwrap().is_none());
    }

    #[test]
    fn changed_binary_is_rejected_before_restarting_under_old_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("identity");
        std::fs::write(&binary, "original").unwrap();
        let evaluator = fixture(configuration(&binary), None);
        std::fs::write(&binary, "different executable with the same reported name").unwrap();
        assert_eq!(
            evaluator.evaluate(&request(), &Running),
            Err(EvaluationFailure::Unavailable)
        );
        assert!(evaluator.worker.lock().unwrap().is_none());
    }

    #[test]
    fn failed_initialization_is_not_an_empty_success() {
        let directory = tempfile::tempdir().unwrap();
        let config = configuration(&directory.path().join("not-needed"));
        let result = Worker::start_script(&config, None, "print('{}', flush=True)");
        assert!(matches!(result, Err(EvaluationFailure::Unavailable)));
    }

    #[cfg(unix)]
    fn assert_not_running(pid: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let output = Command::new("ps")
                .args(["-p", pid, "-o", "stat="])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !output.status.success() || state.is_empty() || state.starts_with('Z') {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "process {pid} remains running: {state}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    #[cfg(unix)]
    fn cancellation_during_handshake_reaps_the_started_process() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let config = configuration(&pid_file);
        let script =
            "import os,sys,time\nopen(sys.argv[1], 'w').write(str(os.getpid()))\ntime.sleep(30)\n";
        struct CancelAfterStart<'a>(&'a Path);
        impl DetectionControl for CancelAfterStart<'_> {
            fn check(&self) -> Result<(), DetectionStop> {
                if std::fs::read_to_string(self.0).is_ok_and(|pid| !pid.is_empty()) {
                    Err(DetectionStop::Cancelled)
                } else {
                    Ok(())
                }
            }
        }
        let result = Worker::start_script(&config, Some(&CancelAfterStart(&pid_file)), script);
        assert!(matches!(
            result,
            Err(EvaluationFailure::Stopped(DetectionStop::Cancelled))
        ));
        assert_not_running(std::fs::read_to_string(pid_file).unwrap().trim());
    }

    #[test]
    #[cfg(unix)]
    fn cancelling_a_blocked_request_kills_the_entire_group() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("child-pid");
        let config = configuration(&pid_file);
        let script = format!(
            "{HELLO}\n{}",
            r#"
import subprocess
child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)'])
open(sys.argv[1], 'w').write(str(child.pid))
for raw in sys.stdin:
    time.sleep(30)
"#
        );
        let (worker, _) = Worker::start_script(&config, None, &script).unwrap();
        // The ready reply precedes the fixture's pid-file write.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pid_file.is_file() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let parent = worker._process.0.id().to_string();
        let child = std::fs::read_to_string(&pid_file).unwrap();
        let evaluator = fixture(config, Some(worker));
        let result = evaluator.evaluate(
            &request(),
            &CancelAt(Instant::now() + Duration::from_millis(100)),
        );
        assert_eq!(
            result,
            Err(EvaluationFailure::Stopped(DetectionStop::Cancelled))
        );
        assert!(evaluator.worker.lock().unwrap().is_none());
        assert_not_running(&parent);
        assert_not_running(child.trim());
    }

    #[test]
    #[ignore = "requires TEMPO_TACTICAL_PYTHON with chess==1.11.2 and TEMPO_STOCKFISH"]
    fn real_engine_recovers_after_cancellation_and_returns_all_legal_roots() {
        let binary = PathBuf::from(std::env::var_os("TEMPO_STOCKFISH").expect("TEMPO_STOCKFISH"));
        let mut config = configuration(&binary);
        config.nodes = 2_000_000;
        let evaluator = StockfishEvaluator::start(config).unwrap();
        let initial_pid = evaluator
            .worker
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            ._process
            .0
            .id();
        let result = evaluator.evaluate(
            &request(),
            &CancelAt(Instant::now() + Duration::from_millis(20)),
        );
        assert_eq!(
            result,
            Err(EvaluationFailure::Stopped(DetectionStop::Cancelled))
        );
        assert!(evaluator.worker.lock().unwrap().is_none());
        #[cfg(unix)]
        assert_not_running(&initial_pid.to_string());
        let evaluation = evaluator.evaluate(&request(), &Running).unwrap();
        assert_eq!(evaluation.provenance, evaluator.descriptor());
        assert!(evaluation.depth >= 12);
        assert!(evaluation.nodes > 0);
        let mut actual = evaluation
            .roots
            .iter()
            .map(|root| root.uci.clone())
            .collect::<Vec<_>>();
        actual.sort();
        assert_eq!(actual, request().legal_moves);
        let capture = evaluation
            .roots
            .iter()
            .find(|root| root.uci == "a1a7")
            .unwrap();
        assert!(
            matches!(capture.score, EvaluationScore::Centipawns(value) | EvaluationScore::Mate(value) if value > 0)
        );
        let restarted_pid = evaluator
            .worker
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            ._process
            .0
            .id();
        assert_ne!(initial_pid, restarted_pid);
    }
}
