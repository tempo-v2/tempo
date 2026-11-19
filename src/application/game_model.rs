//! Read verified authored context and the model evidence applied to it.
use super::{Tempo, content_preview::source_id, repertoires::learner_color};
use crate::game_model::{GapRequest, MODEL_KEY, ModelBook, ModelInput, PROFILE_LABEL};
use crate::{AppError, app_error::domain};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempo_chess::{AuthoredRepertoire, ContentKind, PositionKey};
use tempo_types::DatasetId;

impl Tempo {
    pub(crate) fn model_input(&self, dataset: &DatasetId) -> Result<ModelInput, AppError> {
        let (snapshot, revision) = self.store.content_state()?;
        if &snapshot.dataset != dataset {
            return Err(AppError::new(
                "stale_dataset",
                "The active dataset changed.",
            ));
        }
        let mut targets = BTreeMap::<String, BTreeMap<PositionKey, _>>::new();
        for item in snapshot.catalog.items.values().filter(|item| item.active()) {
            if let ContentKind::Repertoire {
                repertoire,
                position,
            } = &item.definition.kind
            {
                targets
                    .entry(repertoire.clone())
                    .or_default()
                    .insert(position.clone(), item.target.clone());
            }
        }
        let mut books = Vec::new();
        for (id, targets) in targets {
            let doc = self.store.repertoire_document(&id)?;
            if doc.dataset != snapshot.dataset || doc.revision != revision {
                return Err(AppError::new("stale_repertoire", "The repertoire changed."));
            }
            if let (Some(pgn), Some(item)) = (doc.pgn, doc.item) {
                books.push(ModelBook {
                    pgn,
                    color: learner_color(&item),
                    targets,
                });
            }
        }
        Ok(ModelInput {
            dataset: snapshot.dataset,
            revision,
            books,
        })
    }

    pub fn opponent_gaps(&mut self) -> Result<Value, AppError> {
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
                Some(json!({
                    "repertoire_id": group.gap.repertoire_id, "position": group.gap.position,
                    "actual_move": group.gap.actual_move, "games": group.games,
                    "document_revision": document_revision,
                }))
            })
            .take(101)
            .collect::<Vec<_>>();
        Ok(
            json!({"dataset_id":dataset,"gaps":verified.iter().take(100).collect::<Vec<_>>(),
            "more":verified.len() > 100}),
        )
    }

    pub fn game_gap_draft(&self, request: GapRequest) -> Result<Value, AppError> {
        let doc = self.store.repertoire_document(&request.repertoire_id)?;
        if doc.dataset != request.dataset_id {
            return Err(AppError::new(
                "stale_dataset",
                "The active dataset changed.",
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
        let color = learner_color(&item);
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
            "content_revision": doc.revision,
            "profile": PROFILE_LABEL,
            "local_coverage": coverage,
            "actual_probability": actual_probability,
            "top_replies": top,
        }))
    }
}
