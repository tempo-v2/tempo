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
        checked_distribution(position, self.read(Duration::from_secs(10))?)
    }
}

/// Admit a distribution for the position of the requested pinned-model row.
/// A terminal position has no legal move or probability mass to distribute.
pub(crate) fn checked_distribution(
    position: &PositionKey,
    distribution: Distribution,
) -> Result<Distribution, AppError> {
    let legal = legal_model_moves(position).map_err(|error| domain("invalid_position", error))?;
    let expected_mass = if legal.is_empty() { 0.0 } else { 1.0 };
    let mass: f64 = distribution.values().sum();
    if !distribution.keys().eq(legal.iter())
        || distribution.values().any(|p| !p.is_finite() || *p < 0.0)
        || (mass - expected_mass).abs() > 1e-8
    {
        return Err(AppError::new(
            "invalid_maia_reply",
            "Expected a distribution over exactly the legal moves for this position.",
        ));
    }
    Ok(distribution)
}

impl Drop for Maia {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempo_chess::{PositionCanonicalizer, START_FEN, StandardPositionCanonicalizer};

    #[test]
    fn checked_maia_requires_exact_legal_keys_and_mass_with_terminal_empty() {
        let position = StandardPositionCanonicalizer
            .canonicalize(START_FEN)
            .unwrap();
        let moves = legal_model_moves(&position).unwrap();
        let valid = moves
            .into_iter()
            .map(|uci| {
                let probability = if uci == "e2e4" { 1.0 } else { 0.0 };
                (uci, probability)
            })
            .collect::<Distribution>();
        assert_eq!(
            checked_distribution(&position, valid.clone()).unwrap(),
            valid
        );
        let assert_invalid = |cached| {
            assert_eq!(
                checked_distribution(&position, cached).unwrap_err().code,
                "invalid_maia_reply"
            );
        };
        let mut double_mass = valid.clone();
        double_mass.insert("d2d4".into(), 1.0);
        assert_invalid(double_mass);
        let mut illegal = valid.clone();
        illegal.remove("d2d4");
        illegal.insert("z2z4".into(), 0.0);
        assert_invalid(illegal);
        let mut missing = valid.clone();
        missing.remove("d2d4");
        assert_invalid(missing);
        for probability in [-1.0, f64::NAN, f64::INFINITY] {
            let mut bad_probability = valid.clone();
            bad_probability.insert("d2d4".into(), probability);
            assert_invalid(bad_probability);
        }
        assert_invalid(Distribution::new());

        let terminal = StandardPositionCanonicalizer
            .canonicalize("7k/6Q1/6K1/8/8/8/8/8 b - - 0 50")
            .unwrap();
        assert!(
            checked_distribution(&terminal, Distribution::new())
                .unwrap()
                .is_empty()
        );
        assert!(checked_distribution(&terminal, valid).is_err());
    }
}
