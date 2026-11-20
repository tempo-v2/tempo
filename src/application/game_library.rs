//! Archive browsing uses the last recorded comparison. Opening a game in the
//! workbench performs its separate, live comparison against the selected book.
use super::{Tempo, game_evidence::learner_side};
use crate::AppError;
use serde::Deserialize;
use serde_json::{Value, json};
use tempo_chess::{PlayedOpening, played_opening};
use tempo_types::DatasetId;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameLibraryRead {
    dataset_id: DatasetId,
    through: Option<i64>,
    before: Option<i64>,
}

fn opening_name(opening: &PlayedOpening) -> Option<String> {
    if let Some(name) = opening.opening.as_deref().filter(|s| !s.is_empty()) {
        return Some(match &opening.variation {
            Some(variation) if !variation.is_empty() => format!("{name}, {variation}"),
            _ => name.to_string(),
        });
    }
    let slug = opening
        .eco_url
        .as_deref()?
        .strip_prefix("https://www.chess.com/openings/")?
        .split(['/', '?', '#'])
        .next()?;
    (!slug.is_empty()).then(|| slug.replace('-', " "))
}

impl Tempo {
    pub fn game_library(&self, request: GameLibraryRead) -> Result<Value, AppError> {
        let page = self
            .store
            .game_library(&request.dataset_id, request.through, request.before)?;
        let entries: Vec<_> = page.entries.into_iter().map(|row| {
            let g = row.game;
            let color = learner_side(&g, &row.learner);
            let opening = played_opening(&g.pgn).unwrap_or_default();
            json!({
                "source":g.source,"game_id":g.id,"learner":row.learner,"color":color,
                "white":g.white,"black":g.black,"played_ms":g.start_ms.or(g.played_date_ms).or(g.end_ms),
                "opening":opening_name(&opening),"eco":opening.eco,"moves":opening.moves,
                "repertoires":row.repertoires,
            })
        }).collect();
        Ok(
            json!({"dataset_id":request.dataset_id,"through":page.through,"next_before":page.next_before,"entries":entries}),
        )
    }
}
