//! Revision-scoped authored moves and structural comparisons.
use super::ReviewVersion;
use crate::{AppError, app_error::domain, gap_recommendation::SimilarMoves};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempo_chess::{
    AuthoredRepertoire, ContentKind, LearnerColor, PositionKey, SavedRepertoireResponse,
    nearby_repertoire_positions,
};
use tempo_store::{SqliteStore, Store};
use tempo_types::{DatasetId, Revision};

pub(super) struct BookPositions {
    pub(super) marker: (DatasetId, Revision),
    pub(super) scope: (Option<String>, Option<LearnerColor>, bool),
    catalog: BTreeMap<PositionKey, Vec<SavedRepertoireResponse>>,
    names: BTreeMap<String, String>,
    document_id: Option<String>,
    selected_authored: Option<AuthoredRepertoire>,
}
impl BookPositions {
    pub(super) fn load(
        store: &SqliteStore,
        marker: (DatasetId, Revision),
        repertoire_id: Option<&str>,
        learner_color: Option<LearnerColor>,
        game_review: bool,
    ) -> Result<Self, AppError> {
        let mut color = learner_color;
        let mut document_id = None;
        let mut selected_authored = None;
        if let Some(id) = repertoire_id {
            let document = store.repertoire_document(id)?;
            if game_review && document.suspended {
                return Err(AppError::new(
                    "stale_repertoire",
                    "This repertoire is suspended. Reload the game comparison.",
                ));
            }
            document_id = document.source_id;
            let item = document
                .item
                .ok_or_else(|| AppError::new("unknown_repertoire", "Repertoire unavailable."))?;
            color = Some(
                item.definition
                    .learner_color()
                    .map_err(|e| domain("invalid_repertoire", e))?,
            );
        }
        let mut catalog = BTreeMap::<_, Vec<_>>::new();
        let mut names = BTreeMap::new();
        let snapshot = store.content_state()?;
        let revision = snapshot.content_revision;
        if snapshot.dataset != marker.0 || revision != marker.1 {
            return Err(AppError::new(
                "stale_repertoire",
                "The repertoire changed. Refresh the position.",
            ));
        }
        // Active content is authoritative for learner decisions, even for legacy
        // books without a verified PGN. Documents add opponent replies only.
        for item in snapshot.catalog.items.values().filter(|item| item.active()) {
            let ContentKind::Repertoire {
                repertoire,
                position,
            } = &item.definition.kind
            else {
                continue;
            };
            let side = item
                .definition
                .learner_color()
                .map_err(|e| domain("invalid_repertoire", e))?;
            if color.is_some_and(|c| c != side)
                || (game_review && snapshot.suspended_repertoires.contains(repertoire))
            {
                continue;
            }
            catalog
                .entry(position.clone())
                .or_default()
                .push(SavedRepertoireResponse {
                    repertoire_id: repertoire.clone(),
                    answer: item.definition.decisions[0].expected.clone(),
                });
            if names
                .insert(repertoire.clone(), item.label.clone())
                .is_some()
            {
                continue;
            }
            let doc = store.repertoire_document(repertoire)?;
            if let Some(pgn) = doc.pgn {
                let authored =
                    AuthoredRepertoire::parse(&pgn).map_err(|e| domain("invalid_repertoire", e))?;
                for (key, node) in authored.positions.iter().filter(|(_, n)| n.side != side) {
                    for mv in node.replies.keys() {
                        catalog
                            .entry(key.clone())
                            .or_default()
                            .push(SavedRepertoireResponse {
                                repertoire_id: repertoire.clone(),
                                answer: mv.as_str().into(),
                            });
                    }
                }
                if Some(repertoire.as_str()) == repertoire_id {
                    selected_authored = Some(authored);
                }
            }
        }
        Ok(Self {
            marker,
            scope: (repertoire_id.map(str::to_owned), learner_color, game_review),
            catalog,
            names,
            document_id,
            selected_authored,
        })
    }
    pub(super) fn compare(
        &self,
        fen: &str,
        key: &PositionKey,
        expected: Option<&ReviewVersion>,
    ) -> Result<Value, AppError> {
        let id = self.scope.0.as_deref().unwrap_or("");
        if let Some(expected) = expected {
            if expected.revision != self.marker.1
                || self.document_id.as_ref() != Some(&expected.document_id)
                || self.selected_authored.is_none()
            {
                return Err(AppError::new(
                    "stale_repertoire",
                    "The repertoire changed. Reload the game comparison.",
                ));
            }
        }
        let nearby = nearby_repertoire_positions(fen, id, &self.catalog, 8)
            .map_err(|e| domain("invalid_position", e))?;
        let exact = self
            .catalog
            .get(key)
            .into_iter()
            .flatten()
            .map(|entry| json!({"uci":entry.answer.uci,"repertoire_id":entry.repertoire_id}))
            .collect::<Vec<_>>();
        let mut data = json!({"exact":exact,"nearby":nearby,"similar_responses":SimilarMoves::rank(&nearby),
            "repertoire_names":self.names,"content_revision":self.marker.1});
        if expected.is_some() {
            data["dataset_id"] = json!(self.marker.0);
            data["repertoire_id"] = json!(id);
            data["document_id"] = json!(self.document_id);
            // Game review compares the authored document at both learner and opponent positions.
            let moves: Vec<_> = self
                .selected_authored
                .as_ref()
                .and_then(|book| book.positions.get(key))
                .into_iter()
                .flat_map(|node| node.replies.keys())
                .collect();
            data["prepared_uci"] = json!(moves);
        }
        Ok(data)
    }
}
