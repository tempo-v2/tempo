//! Retained authoring request values and model-input identity for saved documents.

use crate::{AppError, app_error::domain};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use tempo_chess::{AuthoredRepertoire, LearnerColor, PositionKey, legal_model_moves};
use tempo_store::{ModelInputStamp, ModelTargets, REACH_MODEL_KEY};
use tempo_types::{DatasetId, Revision};

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GapKind {
    #[default]
    Observed,
    Forecast,
}

/// Authoring authority established by the HTTP ingress, independent of wire data.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthoringSurface {
    Desktop,
    PhoneBuild,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GapRequest {
    pub dataset_id: DatasetId,
    pub repertoire_id: String,
    pub position: PositionKey,
    pub actual_move: String,
    pub document_revision: String,
    pub response: Option<String>,
    #[serde(default)]
    pub gap_kind: GapKind,
    pub report_id: Option<String>,
    pub base_revision: Option<Revision>,
}

pub(crate) fn model_input_stamp(
    authored: &AuthoredRepertoire,
    color: LearnerColor,
    targets: ModelTargets,
) -> ModelInputStamp {
    let routes: BTreeSet<Vec<_>> = authored
        .routes
        .iter()
        .map(|path| {
            path.iter()
                .map(|(before, played)| {
                    (before, played, &authored.positions[before].replies[played])
                })
                .collect()
        })
        .collect();
    let input = serde_json::to_vec(&(
        "tempo-model-input-v1",
        REACH_MODEL_KEY,
        color,
        routes,
        &targets,
    ))
    .expect("serializable repertoire model input");
    ModelInputStamp {
        key: blake3::hash(&input).to_hex().to_string(),
        targets,
    }
}

pub(crate) const MODEL_KEY: &str = "maia2-0.11.0:65aae8465eed5e65df66a24ea7370715579f9e5435098d06fe18bdb1e267e997:rapid:1500:1500:cpu";
pub(crate) const PROFILE_LABEL: &str = "Maia Rapid 1500";
pub(crate) type Distribution = BTreeMap<String, f64>;

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
