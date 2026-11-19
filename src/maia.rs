//! Read-only CPU inference for the pinned Rapid 1500 profile.
use crate::{AppError, app_error::domain};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{Receiver, sync_channel},
    time::Duration,
};
use tempo_chess::{PositionKey, legal_model_moves};
pub(crate) const MODEL_KEY: &str = "maia2-0.11.0:65aae8465eed5e65df66a24ea7370715579f9e5435098d06fe18bdb1e267e997:rapid:1500:1500:cpu";
pub(crate) const PROFILE_LABEL: &str = "Maia Rapid 1500";
pub(crate) type Distribution = BTreeMap<String, f64>;
pub(crate) struct Maia {
    child: Child,
    input: ChildStdin,
    output: Receiver<String>,
}

impl Maia {
    pub(crate) fn start(directory: &Path) -> Result<Self, AppError> {
        let mut child = Command::new(directory.join("venv/bin/python"))
            .args(["-I", "-u", "-c", include_str!("../tools/maia/infer.py")])
            .arg(directory.join("weights"))
            .env("OMP_NUM_THREADS", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| domain("maia_runtime_unavailable", error))?;
        let input = child.stdin.take().expect("piped model input");
        let stdout = child.stdout.take().expect("piped model output");
        let (send, output) = sync_channel(1);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let process = Self {
            child,
            input,
            output,
        };
        let ready: Value = process.read(Duration::from_secs(30))?;
        if ready["ready"] != true {
            return Err(AppError::new(
                "maia_runtime_unavailable",
                "The Maia worker did not initialize.",
            ));
        }
        Ok(process)
    }

    fn read<T: serde::de::DeserializeOwned>(&self, timeout: Duration) -> Result<T, AppError> {
        let reply = self
            .output
            .recv_timeout(timeout)
            .map_err(|error| domain("maia_timeout", error))?;
        serde_json::from_str(&reply).map_err(|error| domain("invalid_maia_reply", error))
    }

    pub(crate) fn infer(&mut self, position: &PositionKey) -> Result<Distribution, AppError> {
        let fen = json!(format!("{} 0 1", position.0));
        writeln!(self.input, "{fen}").map_err(|error| domain("maia_request", error))?;
        let distribution: Distribution = self.read(Duration::from_secs(10))?;
        let legal =
            legal_model_moves(position).map_err(|error| domain("invalid_position", error))?;
        let mass: f64 = distribution.values().sum();
        if !distribution.keys().eq(legal.iter())
            || distribution.values().any(|p| !p.is_finite() || *p < 0.0)
            || (mass - 1.0).abs() > 1e-8
        {
            return Err(AppError::new(
                "invalid_maia_reply",
                "Expected one probability for every legal move.",
            ));
        }
        Ok(distribution)
    }
}

impl Drop for Maia {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
