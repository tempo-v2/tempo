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
    Catalog, DetectorIdentity, TacticalAssessment, TacticalOutcome, parse_played_games,
    prepare_personal_puzzle, prepare_tactical_puzzle,
};
use tempo_learning::DailyLearning;
use tempo_store::{
    TacticalAcceptance, TacticalAcceptanceBasis, TacticalAnalysis, TacticalAnalysisStatus,
    TacticalCommit, TacticalGame, TacticalObservation, TacticalPuzzleRef, TacticalTargetChange,
};
use tempo_types::{DatasetId, TargetId};

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

enum DetectionPuzzle<'a> {
    Personal(&'a tempo_chess::PersonalPuzzle),
    Capture(&'a str, &'a tempo_chess::ChessAnswer),
}

impl Tempo {
    fn validate_tactical_source(
        &self,
        game: &TacticalGame,
        completion: &DetectionCompletion,
    ) -> Result<TacticalAcceptanceBasis, AppError> {
        let _trace = crate::diagnostics::Span::new("tactics.acceptance.basis");
        let basis = self.store.tactical_acceptance_basis()?;
        if completion.request.dataset_id != basis.dataset {
            return Err(AppError::new(
                "stale_dataset",
                "The analyzed dataset has changed.",
            ));
        }
        let current = self
            .store
            .stored_game(&basis.dataset, &game.game.source, &game.game.id)?
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
        Ok(basis)
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
        let _trace = crate::diagnostics::Span::new("tactics.acceptance.record_failure");
        let basis = self.validate_tactical_source(game, completion)?;
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
            TacticalCommit {
                expected_revision: basis.revision,
                expected_dataset: basis.dataset,
                targets: BTreeMap::new(),
            },
            &analysis,
        )?)
    }

    pub(crate) fn accept_tactical_analysis(
        &mut self,
        game: &TacticalGame,
        completion: &DetectionCompletion,
    ) -> Result<TacticalAcceptance, AppError> {
        let mut trace = crate::diagnostics::Span::new("tactics.acceptance");
        let basis = self.validate_tactical_source(game, completion)?;
        let dataset = &basis.dataset;
        let at = self.components.clock.now();
        let mut targets: BTreeMap<TargetId, TacticalTargetChange> = BTreeMap::new();
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
                    let ply = moment
                        .occurrence
                        .try_into()
                        .map_err(|_| AppError::new("invalid_game", "Game is too long."))?;
                    let motif = assessment.kind.code();
                    let producer_id = &finding.producer.id;
                    let address = if personal {
                        tempo_store::TacticalRequestAddress::Decision {
                            ply,
                            motif,
                            producer_id,
                        }
                    } else {
                        tempo_store::TacticalRequestAddress::Finding {
                            ply,
                            motif,
                            producer_id,
                        }
                    };
                    let personal_puzzle = if personal {
                        assessment
                            .personal
                            .as_ref()
                            .ok_or_else(|| {
                                AppError::new(
                                    "invalid_tactical_result",
                                    "The personal detector omitted its evidence.",
                                )
                            })?
                            .puzzle
                            .as_ref()
                    } else {
                        None
                    };
                    let proposal = if personal {
                        personal_puzzle.map(DetectionPuzzle::Personal)
                    } else {
                        assessment
                            .puzzle
                            .as_ref()
                            .map(|puzzle| DetectionPuzzle::Capture(&puzzle.fen, &puzzle.answer))
                    };
                    let train = if let Some(proposal) = proposal {
                        let binding = self.store.tactical_training_binding(
                            dataset,
                            game,
                            GAME_TACTICS_REPORT,
                            address,
                        )?;
                        let mut plan_trace =
                            crate::diagnostics::Span::new("tactics.acceptance.plan");
                        plan_trace.field(
                            "phase",
                            if binding.is_some() {
                                "retained_binding"
                            } else {
                                "new_binding"
                            },
                        );
                        let prepared = match proposal {
                            DetectionPuzzle::Personal(puzzle) => {
                                prepare_personal_puzzle(puzzle, &provenance)
                            }
                            DetectionPuzzle::Capture(fen, answer) => {
                                prepare_tactical_puzzle(fen, answer, &provenance)
                            }
                        };
                        let plan = match prepared {
                            Ok(puzzle) => {
                                let matched = self
                                    .store
                                    .tactical_content_matches(dataset, &puzzle.lookup())?;
                                let mut catalog = Catalog {
                                    items: matched
                                        .into_iter()
                                        .map(|item| (item.target.clone(), item))
                                        .collect(),
                                    lines: Vec::new(),
                                };
                                catalog.items.extend(targets.iter().filter_map(
                                    |(target, change)| {
                                        change.content.clone().map(|item| (target.clone(), item))
                                    },
                                ));
                                puzzle.plan(&catalog).map(|mut plan| {
                                    let item = plan
                                        .content
                                        .catalog
                                        .items
                                        .remove(&plan.target)
                                        .expect("planned tactic");
                                    let changed = catalog.items.get(&item.target) != Some(&item);
                                    (item, changed)
                                })
                            }
                            Err(error) => Err(error),
                        };
                        if let Some(binding) = binding {
                            // Legacy receipts can include the detached proposal.
                            // Its resolution changes no retained content or Learning.
                            plan.ok()
                                .map(|(item, _)| TacticalPuzzleRef {
                                    target: item.target,
                                    version: item.version,
                                })
                                .or(Some(binding))
                        } else {
                            let (item, changed) =
                                plan.map_err(|error| domain("invalid_tactical_puzzle", error))?;
                            let staged = targets.entry(item.target.clone()).or_default();
                            if staged.introduction.is_none() {
                                let prior = self
                                    .store
                                    .tactical_target_learning(dataset, &item.target)?
                                    .unwrap_or_default();
                                if !prior.introduced {
                                    staged.introduction = Some(
                                        DailyLearning::introduce_capture(&prior, at)
                                            .map_err(|error| domain("learning_error", error))?,
                                    );
                                }
                            }
                            let puzzle = TacticalPuzzleRef {
                                target: item.target.clone(),
                                version: item.version,
                            };
                            if changed {
                                staged.content = Some(item);
                            }
                            Some(puzzle)
                        }
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
        targets.retain(|_, change| change.content.is_some() || change.introduction.is_some());
        trace.field("count", analysis.observations.len());
        let mut write_trace = crate::diagnostics::Span::new("tactics.acceptance.persist");
        write_trace.field("count", targets.len());
        let accepted = self.store.commit_tactical_analysis(
            TacticalCommit {
                expected_revision: basis.revision,
                expected_dataset: basis.dataset,
                targets,
            },
            &analysis,
        )?;
        drop(write_trace);
        trace.set_outcome(if accepted.replayed {
            "replayed"
        } else {
            "committed"
        });
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
