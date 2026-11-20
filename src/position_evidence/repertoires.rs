//! Revision-scoped authored moves and structural comparisons.
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
    pub(super) scope: (Option<String>, Option<LearnerColor>),
    catalog: BTreeMap<PositionKey, Vec<SavedRepertoireResponse>>,
    names: BTreeMap<String, String>,
}
impl BookPositions {
    pub(super) fn load(
        store: &SqliteStore,
        marker: (DatasetId, Revision),
        repertoire_id: Option<&str>,
        learner_color: Option<LearnerColor>,
    ) -> Result<Self, AppError> {
        let mut color = learner_color;
        if let Some(id) = repertoire_id {
            let item = store
                .repertoire_document(id)?
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
            if color.is_some_and(|c| c != side) {
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
                for (key, node) in authored
                    .positions
                    .into_iter()
                    .filter(|(_, n)| n.side != side)
                {
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
            }
        }
        Ok(Self {
            marker,
            scope: (repertoire_id.map(str::to_owned), learner_color),
            catalog,
            names,
        })
    }
    pub(super) fn compare(&self, fen: &str, key: &PositionKey) -> Result<Value, AppError> {
        let id = self.scope.0.as_deref().unwrap_or("");
        let nearby = nearby_repertoire_positions(fen, id, &self.catalog, 8)
            .map_err(|e| domain("invalid_position", e))?;
        let exact = self
            .catalog
            .get(key)
            .into_iter()
            .flatten()
            .map(|entry| json!({"uci":entry.answer.uci,"repertoire_id":entry.repertoire_id}))
            .collect::<Vec<_>>();
        Ok(
            json!({"exact":exact,"nearby":nearby,"similar_responses":SimilarMoves::rank(&nearby),
            "repertoire_names":self.names,"content_revision":self.marker.1}),
        )
    }
}
