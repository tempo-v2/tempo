//! Accept typed game observations and their puzzle requests in one transaction.
use super::Tempo;
use crate::{
    AppError,
    app_error::domain,
    game_detection::{DetectionCompletion, DetectionStatus},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempo_chess::{
    DetectorIdentity, TacticalAssessment, TacticalOutcome, parse_played_games,
    plan_personal_puzzle, plan_tactical_puzzle,
};
use tempo_learning::DailyLearning;
use tempo_store::{
    Commit, TacticalAcceptance, TacticalAnalysis, TacticalAnalysisStatus, TacticalGame,
    TacticalObservation, TacticalPuzzleRef, Write,
};
use tempo_types::DatasetId;

// A report groups game tactics across upgrades and changes in its detector set.
pub(crate) const GAME_TACTICS_REPORT: &str = "game-tactics";

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct TacticalDetails {
    pub(crate) schema_version: u8,
    pub(crate) fen: String,
    pub(crate) position: String,
    pub(crate) side_to_move: tempo_chess::LearnerColor,
    pub(crate) assessment: TacticalAssessment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) opponent_lead_in: Option<tempo_chess::OpponentLeadIn>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) skipped_analyses: Vec<String>,
}

pub(crate) fn analysis_version(analyses: &[DetectorIdentity]) -> String {
    let mut members = analyses.to_vec();
    members.sort();
    let input = json!(["tactical-acceptance-v2-position-set", members]);
    blake3::hash(input.to_string().as_bytes())
        .to_hex()
        .to_string()
}

pub(crate) fn game_learner(game: &TacticalGame) -> Result<tempo_chess::LearnerColor, AppError> {
    super::game_evidence::learner_side(&game.game, &game.learner).ok_or_else(|| {
        AppError::new(
            "invalid_perspective",
            "Expected a verified game participant.",
        )
    })
}

impl Tempo {
    fn validate_tactical_source(
        &self,
        game: &TacticalGame,
        completion: &DetectionCompletion,
    ) -> Result<DatasetId, AppError> {
        let dataset = self.game_dataset()?;
        if completion.request.dataset_id != dataset {
            return Err(AppError::new(
                "stale_dataset",
                "The analyzed dataset has changed.",
            ));
        }
        let current = self
            .store
            .stored_game(&dataset, &game.game.source, &game.game.id)?
            .ok_or_else(|| AppError::new("stale_source", "The analyzed game is unavailable."))?;
        if current.mainline_signature != game.game.mainline_signature
            || current.pgn != completion.source.pgn
            || completion.request.source != game.game.source
            || completion.request.game_id != game.game.id
            || completion.request.learner != game.learner
        {
            return Err(AppError::new(
                "stale_source",
                "The analyzed game has changed.",
            ));
        }
        Ok(dataset)
    }

    /// Persist an application composition failure without applying any of the
    /// rejected puzzle, learning, or observation changes. A failed game must not
    /// block the remaining archive, and its result must not look like an empty
    /// successful scan.
    pub(crate) fn record_tactical_failure(
        &mut self,
        game: &TacticalGame,
        completion: &DetectionCompletion,
        error: &AppError,
    ) -> Result<TacticalAcceptance, AppError> {
        let dataset = self.validate_tactical_source(game, completion)?;
        let snapshot = self.store.study_state(Default::default())?;
        if snapshot.dataset != dataset {
            return Err(AppError::new(
                "stale_dataset",
                "The analyzed dataset has changed.",
            ));
        }
        let analysis = TacticalAnalysis {
            position: completion.position,
            source: game.game.source.clone(),
            game_id: game.game.id.clone(),
            learner: game.learner.clone(),
            report: GAME_TACTICS_REPORT.into(),
            analysis_version: analysis_version(&completion.analyses),
            game_fingerprint: game.game.mainline_signature.clone(),
            analyzed_at_ms: self.components.clock.now().unix_ms,
            decisions_scanned: 0,
            status: TacticalAnalysisStatus::Failed,
            error: Some(format!(
                "The tactical result could not be accepted: {}: {}",
                error.code, error.message
            )),
            observations: Vec::new(),
        };
        Ok(self.store.commit_tactical_analysis(
            Commit {
                expected_revision: snapshot.revision,
                expected_dataset: dataset,
                writes: Vec::new(),
            },
            &analysis,
        )?)
    }

    pub(crate) fn accept_tactical_analysis(
        &mut self,
        game: &TacticalGame,
        completion: &DetectionCompletion,
    ) -> Result<TacticalAcceptance, AppError> {
        let dataset = self.validate_tactical_source(game, completion)?;
        let needs_puzzle = matches!(&completion.status, DetectionStatus::Complete { findings, .. }
            if findings.iter().any(|finding| finding.moment.tactical.as_ref().is_some_and(|assessment| assessment.puzzle.is_some() || assessment.personal.as_ref().is_some_and(|evidence| evidence.puzzle.is_some()))));
        let snapshot = needs_puzzle
            .then(|| self.store.content_state())
            .transpose()?;
        let revision = if let Some(snapshot) = &snapshot {
            snapshot.revision
        } else {
            self.store.study_state(Default::default())?.revision
        };
        let at = self.components.clock.now();
        let mut catalog = snapshot
            .as_ref()
            .map(|snapshot| snapshot.catalog.clone())
            .unwrap_or_default();
        let mut learning = BTreeMap::new();
        let mut observations = Vec::new();
        let mut decisions_scanned = 0;
        let (status, error) = match &completion.status {
            DetectionStatus::Complete {
                findings,
                learner_color,
                ..
            } => {
                if completion.position.is_some() {
                    decisions_scanned = 1;
                } else {
                    let mut games = parse_played_games(&completion.source.pgn)
                        .map_err(|e| domain("invalid_game", e))?;
                    if games.len() != 1 {
                        return Err(AppError::new("invalid_game", "Expected one analyzed game."));
                    }
                    let played = games
                        .pop()
                        .ok_or_else(|| AppError::new("invalid_game", "Missing analyzed game."))?
                        .map_err(|e| domain("invalid_game", e))?;
                    decisions_scanned = played
                        .timeline()
                        .occurrences()
                        .iter()
                        .filter(|p| p.side_to_move() == *learner_color && p.next_move().is_some())
                        .count()
                        .try_into()
                        .map_err(|_| AppError::new("invalid_game", "Game is too long."))?;
                }
                let mut inconclusive = false;
                for finding in findings {
                    let moment = &finding.moment;
                    let assessment = moment.tactical.as_ref().ok_or_else(|| {
                        AppError::new(
                            "invalid_tactical_result",
                            "The position analysis omitted its typed assessment.",
                        )
                    })?;
                    inconclusive |= assessment.outcome == TacticalOutcome::Uncertain
                        || assessment
                            .standalone_analysis
                            .as_ref()
                            .is_some_and(|value| {
                                !matches!(value, tempo_chess::EvaluationStatus::Complete { .. })
                            });
                    let provenance = [format!(
                        "game-tactic:{}:{}:{}:{}",
                        game.game.source, game.game.id, game.learner, moment.occurrence
                    )];
                    let personal = assessment.kind == tempo_chess::TacticalKind::PersonalTactics;
                    let plan = if personal {
                        let evidence = assessment.personal.as_ref().ok_or_else(|| {
                            AppError::new(
                                "invalid_tactical_result",
                                "The personal detector omitted its evidence.",
                            )
                        })?;
                        evidence
                            .puzzle
                            .as_ref()
                            .map(|puzzle| plan_personal_puzzle(&catalog, puzzle, &provenance))
                    } else {
                        assessment.puzzle.as_ref().map(|puzzle| {
                            plan_tactical_puzzle(&catalog, &puzzle.fen, &puzzle.answer, &provenance)
                        })
                    }
                    .transpose()
                    .map_err(|e| domain("invalid_tactical_puzzle", e))?;
                    let ply = moment
                        .occurrence
                        .try_into()
                        .map_err(|_| AppError::new("invalid_game", "Game is too long."))?;
                    let motif = assessment.kind.code();
                    let address = if personal {
                        tempo_store::TacticalRequestAddress::Decision { ply }
                    } else {
                        tempo_store::TacticalRequestAddress::Finding {
                            ply,
                            motif,
                            producer_id: &finding.producer.id,
                        }
                    };
                    let issued = if plan.is_some() {
                        self.store.issued_tactical_request(
                            &dataset,
                            game,
                            GAME_TACTICS_REPORT,
                            address,
                        )?
                    } else {
                        None
                    };
                    let train = if let Some((plan, issued)) = plan.as_ref().zip(issued.as_ref()) {
                        let current = plan
                            .content
                            .catalog
                            .items
                            .get(&plan.target)
                            .expect("planned puzzle exists");
                        let proposed = TacticalPuzzleRef {
                            target: plan.target.clone(),
                            version: current.version,
                        };
                        // An issued Finding keeps its original binding. Preserve
                        // this immutable report's proposal for acceptance replay.
                        // Decision reports already expose a revised issue as superseded.
                        (!personal || proposed == *issued).then_some(proposed)
                    } else if let Some(plan) = plan {
                        catalog = plan.content.catalog;
                        let item = catalog
                            .items
                            .get(&plan.target)
                            .expect("planned puzzle exists");
                        let prior = learning
                            .get(&plan.target)
                            .or_else(|| snapshot.as_ref()?.learning.get(&plan.target))
                            .cloned()
                            .unwrap_or_default();
                        if !prior.introduced {
                            let introduced = DailyLearning::introduce_capture(&prior, at)
                                .map_err(|e| domain("learning_error", e))?;
                            learning.insert(plan.target.clone(), introduced);
                        }
                        Some(TacticalPuzzleRef {
                            target: plan.target,
                            version: item.version,
                        })
                    } else {
                        None
                    };
                    let details = TacticalDetails {
                        schema_version: 1,
                        fen: moment.fen.clone(),
                        position: moment.position.0.clone(),
                        side_to_move: moment.side_to_move,
                        assessment: assessment.clone(),
                        opponent_lead_in: moment.opponent_lead_in.clone(),
                        skipped_analyses: finding
                            .skipped
                            .iter()
                            .map(|member| member.id.clone())
                            .collect(),
                    };
                    observations.push(TacticalObservation {
                        producer: Some(finding.producer.clone()),
                        ply,
                        motif: motif.into(),
                        outcome: serde_json::to_value(assessment.outcome)
                            .expect("typed outcome")
                            .as_str()
                            .expect("enum string")
                            .into(),
                        details_json: serde_json::to_string(&details)
                            .expect("serializable tactical details"),
                        train,
                        request_scope: if personal {
                            tempo_store::TacticalRequestScope::Decision
                        } else {
                            tempo_store::TacticalRequestScope::Finding
                        },
                    });
                }
                (
                    if inconclusive {
                        TacticalAnalysisStatus::Inconclusive
                    } else {
                        TacticalAnalysisStatus::Complete
                    },
                    None,
                )
            }
            DetectionStatus::TimedOut => (
                TacticalAnalysisStatus::TimedOut,
                Some("The bounded game scan timed out.".into()),
            ),
            DetectionStatus::Unsupported => (
                TacticalAnalysisStatus::Unsupported,
                Some("The detector does not support this game.".into()),
            ),
            DetectionStatus::SourceUnavailable => {
                return Err(AppError::new(
                    "stale_source",
                    "The analyzed source has changed.",
                ));
            }
            DetectionStatus::Running => {
                return Err(AppError::new(
                    "analysis_running",
                    "The game scan has not completed.",
                ));
            }
            _ => (
                TacticalAnalysisStatus::Failed,
                Some("The detector did not produce a complete valid result.".into()),
            ),
        };
        let mut writes = Vec::new();
        if snapshot.is_some_and(|snapshot| catalog != snapshot.catalog) {
            writes.push(Write::Catalog(catalog));
        }
        writes.extend(
            learning
                .into_iter()
                .map(|(target, state)| Write::Learning { target, state }),
        );
        let analysis = TacticalAnalysis {
            position: completion.position,
            source: game.game.source.clone(),
            game_id: game.game.id.clone(),
            learner: game.learner.clone(),
            report: GAME_TACTICS_REPORT.into(),
            analysis_version: analysis_version(&completion.analyses),
            game_fingerprint: game.game.mainline_signature.clone(),
            analyzed_at_ms: at.unix_ms,
            decisions_scanned,
            status,
            error,
            observations,
        };
        let accepted = self.store.commit_tactical_analysis(
            Commit {
                expected_revision: revision,
                expected_dataset: dataset,
                writes,
            },
            &analysis,
        )?;
        if let Some(daily) = &mut self.daily {
            for target in &accepted.new_request_targets {
                daily.queue.clear_deferral(target);
            }
        }
        Ok(accepted)
    }

    pub(crate) fn tactical_history(
        &self,
        dataset: &DatasetId,
        version: &str,
        before: Option<u64>,
    ) -> Result<Value, AppError> {
        if &self.game_dataset()? != dataset {
            return Err(AppError::new(
                "stale_dataset",
                "Reload the current game archive.",
            ));
        }
        let statistics = self
            .store
            .tactical_statistics(dataset, GAME_TACTICS_REPORT, version)?;
        let page =
            self.store
                .tactical_observations(dataset, GAME_TACTICS_REPORT, version, before, 30)?;
        let mut observations = Vec::new();
        for stored in page.observations {
            let details: TacticalDetails = serde_json::from_str(&stored.observation.details_json)
                .map_err(|e| domain("invalid_tactical_history", e))?;
            observations.push(json!({
                "id": stored.id, "source": stored.source, "game_id": stored.game_id,
                "learner": stored.learner, "ply": stored.observation.ply,
                "outcome": details.assessment.outcome,
                "details": details, "puzzle": stored.observation.train,
                "producer": stored.observation.producer,
                "request_id": stored.request_id, "fulfilled": stored.fulfilled,
                "analysis_version": stored.analysis_version,
            }));
        }
        Ok(
            json!({"statistics": statistics, "observations": observations,
            "next_before": page.next_before, "analysis_version": version}),
        )
    }
}

#[cfg(test)]
mod tests;
