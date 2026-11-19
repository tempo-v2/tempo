//! Read verified authored context and the model evidence applied to it.
use super::{Tempo, content_preview::source_id};
use crate::game_model::{GapRequest, model_input_stamp};
use crate::maia::{MODEL_KEY, PROFILE_LABEL};
use crate::{
    AppError, app_error::domain, commands::RepertoireInput, gap_analysis::GapAnalysisInput,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempo_chess::{
    AuthoredRepertoire, ContentKind, GapDraft, LearnerColor, PositionKey, SavedRepertoireResponse,
    compile_repertoire, legal_move_labels,
};
use tempo_store::{ModelAuthority, ProjectionStatus, StoreError, model_lines, model_targets};
use tempo_types::Revision;

struct CheckedGap {
    authored: AuthoredRepertoire,
    draft: GapDraft,
    revision: Revision,
    name: String,
    color: LearnerColor,
}

impl Tempo {
    pub(crate) fn model_projection_status(&self) -> Result<Option<ProjectionStatus>, AppError> {
        Ok(self
            .store
            .game_model_projection_status(&self.game_dataset()?)?)
    }

    /// Verify legacy source associations once against accepted content before cache reprojection.
    pub fn bootstrap_model_inputs(&mut self) -> Result<(), AppError> {
        for _ in 0..4 {
            let dataset = self.game_dataset()?;
            let pending = self.store.model_bootstrap_repertoires(&dataset)?;
            if pending.is_empty() {
                return Ok(());
            }
            let snapshot = self.store.content_state()?;
            let revision = snapshot.content_revision;
            if snapshot.dataset != dataset {
                continue;
            }
            for id in pending {
                let doc = self.store.repertoire_document(&id)?;
                if doc.dataset != dataset || doc.revision != revision {
                    break;
                }
                let (Some(source_id), ModelAuthority::NeedsBootstrap) =
                    (doc.source_id, doc.model_authority)
                else {
                    continue;
                };
                let stamp = doc
                    .pgn
                    .as_deref()
                    .zip(doc.item.as_ref())
                    .and_then(|(pgn, item)| {
                        let color = item.definition.learner_color().ok()?;
                        let compiled =
                            compile_repertoire(pgn, &id, &item.label, color, &source_id).ok()?;
                        let targets = model_targets(&snapshot.catalog, &id);
                        let compiled_targets = model_targets(&compiled, &id);
                        if model_lines(&compiled, &id) != model_lines(&snapshot.catalog, &id)
                            || !compiled_targets
                                .iter()
                                .map(|(id, (_, definition))| (id, definition))
                                .eq(targets.iter().map(|(id, (_, definition))| (id, definition)))
                        {
                            return None;
                        }
                        let authored = AuthoredRepertoire::parse(pgn).ok()?;
                        Some(model_input_stamp(&authored, color, targets))
                    });
                match self.store.finish_model_bootstrap(
                    &dataset,
                    revision,
                    &id,
                    &source_id,
                    stamp.as_ref(),
                ) {
                    Ok(()) => {}
                    Err(StoreError::Conflict(_)) => break,
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Err(AppError::new(
            "stale_repertoire",
            "The repertoire changed during model source verification.",
        ))
    }

    pub fn opponent_gaps(&mut self, offset: usize) -> Result<Value, AppError> {
        let dataset = self.game_dataset()?;
        let candidates = self.store.opponent_gaps(&dataset)?;
        if candidates.is_empty() {
            return Ok(json!({"dataset_id":dataset,"gaps":[]}));
        }
        let books = self.current_game_books(&dataset)?;
        let verified = candidates
            .into_iter()
            .filter_map(|group| {
                let document_revision = books.document_for_gap(&group.gap)?;
                Some((group, document_revision))
            })
            .skip(offset)
            .take(101)
            .map(|(group, document_revision)| {
                let labels = legal_move_labels(&format!("{} 0 1", group.gap.position.0)).ok();
                let san = labels
                    .as_ref()
                    .and_then(|moves| moves.get(&group.gap.actual_move));
                json!({
                    "repertoire_id": group.gap.repertoire_id, "position": group.gap.position,
                    "actual_move": group.gap.actual_move, "actual_san": san, "games": group.games,
                    "document_revision": document_revision,
                })
            })
            .collect::<Vec<_>>();
        Ok(
            json!({"dataset_id":dataset,"gaps":verified.iter().take(100).collect::<Vec<_>>(),
            "more":verified.len() > 100}),
        )
    }

    fn checked_gap(&self, request: &GapRequest) -> Result<CheckedGap, AppError> {
        let doc = self.store.repertoire_document(&request.repertoire_id)?;
        if doc.dataset != request.dataset_id {
            return Err(AppError::new(
                "stale_dataset",
                "The active dataset changed.",
            ));
        }
        if request
            .base_revision
            .is_some_and(|revision| revision != doc.revision)
        {
            return Err(AppError::new(
                "stale_draft",
                "The repertoire changed. Reopen the gap before saving.",
            ));
        }
        let (Some(pgn), Some(item)) = (doc.pgn, doc.item) else {
            return Err(AppError::new(
                "unavailable_repertoire",
                "There is no verified current repertoire document.",
            ));
        };
        if source_id(&pgn) != request.document_revision {
            return Err(AppError::new(
                "stale_repertoire",
                "The repertoire changed. Refresh game evidence before editing.",
            ));
        }
        let color = item
            .definition
            .learner_color()
            .map_err(|error| domain("invalid_content", error))?;
        let authored =
            AuthoredRepertoire::parse(&pgn).map_err(|error| domain("invalid_repertoire", error))?;
        let draft = authored
            .gap_draft(
                &pgn,
                &request.position,
                &request.actual_move,
                color,
                request.response.as_deref(),
            )
            .map_err(|error| domain("stale_gap", error))?;
        Ok(CheckedGap {
            authored,
            draft,
            revision: doc.revision,
            name: item.label,
            color,
        })
    }

    pub(crate) fn gap_position(
        &self,
        mut request: GapRequest,
    ) -> Result<(GapDraft, Revision), AppError> {
        request.response = None;
        let CheckedGap {
            draft, revision, ..
        } = self.checked_gap(&request)?;
        Ok((draft, revision))
    }

    pub(crate) fn gap_analysis_input(
        &self,
        request: GapRequest,
    ) -> Result<GapAnalysisInput, AppError> {
        let dataset = request.dataset_id.clone();
        let repertoire_id = request.repertoire_id.clone();
        let (draft, revision) = self.gap_position(request)?;
        let snapshot = self.store.content_state()?;
        let current_revision = snapshot.content_revision;
        if snapshot.dataset != dataset || current_revision != revision {
            return Err(AppError::new(
                "stale_repertoire",
                "The repertoire changed. Open the gap again.",
            ));
        }
        let mut catalog = BTreeMap::<PositionKey, Vec<SavedRepertoireResponse>>::new();
        let mut repertoire_names = BTreeMap::new();
        for item in snapshot.catalog.items.values().filter(|item| item.active()) {
            if let ContentKind::Repertoire {
                position,
                repertoire,
            } = &item.definition.kind
            {
                repertoire_names
                    .entry(repertoire.clone())
                    .or_insert_with(|| item.label.clone());
                catalog
                    .entry(position.clone())
                    .or_default()
                    .push(SavedRepertoireResponse {
                        repertoire_id: repertoire.clone(),
                        answer: item.definition.decisions[0].expected.clone(),
                    });
            }
        }
        Ok(GapAnalysisInput {
            draft,
            catalog,
            repertoire_id,
            repertoire_names,
            revision,
        })
    }

    pub fn game_gap_draft(&self, request: GapRequest) -> Result<Value, AppError> {
        let CheckedGap {
            authored,
            draft,
            revision,
            name,
            color,
        } = self.checked_gap(&request)?;
        let mut anchor_route = authored.positions[&request.position].prefix.clone();
        anchor_route.push(request.actual_move.clone());
        let save_token = draft
            .pgn
            .as_ref()
            .map(|pgn| {
                self.gap_response_token(
                    &RepertoireInput {
                        dataset_id: request.dataset_id.clone(),
                        repertoire_id: Some(request.repertoire_id.clone()),
                        name: name.clone(),
                        learner_color: color,
                        pgn: pgn.clone(),
                    },
                    &request.repertoire_id,
                    revision,
                )
            })
            .transpose()?;
        let before = legal_move_labels(&format!("{} 0 1", request.position.0))
            .map_err(|error| domain("invalid_gap_position", error))?;
        let after =
            legal_move_labels(&draft.fen).map_err(|error| domain("invalid_gap_position", error))?;
        let distribution = self
            .store
            .game_move_distribution(MODEL_KEY, &request.position.0)?;
        let coverage = distribution.as_ref().map(|moves| {
            authored.positions[&request.position]
                .replies
                .iter()
                .filter(|(_, after)| {
                    authored
                        .positions
                        .get(*after)
                        .is_some_and(|node| !node.replies.is_empty())
                })
                .filter_map(|(played, _)| moves.get(played))
                .sum::<f64>()
        });
        let actual_probability = distribution
            .as_ref()
            .and_then(|moves| moves.get(&request.actual_move));
        let top: Option<Vec<_>> = distribution.as_ref().map(|moves| {
            let mut ranked: Vec<_> = moves.iter().collect();
            ranked.sort_by(|a, b| b.1.total_cmp(a.1).then_with(|| a.0.cmp(b.0)));
            ranked.truncate(5);
            ranked
        });
        Ok(json!({
            "draft": draft,
            "anchor_route": anchor_route,
            "content_revision": revision,
            "save_token": save_token,
            "repertoire_name": name,
            "learner_color": match color { LearnerColor::White => "white", LearnerColor::Black => "black" },
            "actual_san": before.get(&request.actual_move),
            "opponent_move_labels": before,
            "response_move_labels": after,
            "profile": PROFILE_LABEL,
            "local_coverage": coverage,
            "actual_probability": actual_probability,
            "top_replies": top,
        }))
    }
}
