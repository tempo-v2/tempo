//! Compare an accepted mainline to today's selected repertoire without processing another game.
use super::{
    Tempo,
    game_evidence::{learner_side, parse_stored_game},
};
use crate::{AppError, app_error::domain};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempo_chess::{
    AuthoredRepertoire, PositionCanonicalizer, PositionKey, RepertoireAdherence,
    StandardPositionCanonicalizer, apply_setup_move,
};
use tempo_types::DatasetId;

/// Read-only address. Omitting the destination requests the played timeline alone.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameReviewRead {
    pub dataset_id: DatasetId,
    pub source: String,
    pub game_id: String,
    pub learner: String,
    pub repertoire_id: Option<String>,
    pub line: Option<GameReviewLine>,
}

/// An exploratory UCI continuation from a zero-based board on the accepted mainline.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameReviewLine {
    pub from_ply: usize,
    pub moves: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Preparation {
    Position,
    Endpoint,
    Outside,
    Unavailable,
}

fn preparation<'a>(
    book: Option<&'a AuthoredRepertoire>,
    key: &PositionKey,
) -> (Preparation, Vec<&'a String>) {
    let Some(book) = book else {
        return (Preparation::Unavailable, Vec::new());
    };
    match book.positions.get(key) {
        None => (Preparation::Outside, Vec::new()),
        Some(node) if node.replies.is_empty() => (Preparation::Endpoint, Vec::new()),
        Some(node) => (Preparation::Position, node.replies.keys().collect()),
    }
}

// A PGN collection can have different starting boards. Only show a second
// route when its edges connect this game's start to the same canonical board.
fn other_route<'a>(
    book: &'a AuthoredRepertoire,
    start: &PositionKey,
    key: &PositionKey,
    played: &[&str],
) -> Option<&'a [String]> {
    let prefix = &book.positions.get(key)?.prefix;
    if prefix == played {
        return None;
    }
    let end = prefix
        .iter()
        .try_fold(start, |at, uci| book.positions.get(at)?.replies.get(uci))?;
    (end == key).then_some(prefix)
}

impl Tempo {
    pub fn game_review(&self, request: GameReviewRead) -> Result<Value, AppError> {
        if self.game_dataset()? != request.dataset_id {
            return Err(AppError::new(
                "stale_dataset",
                "The selected dataset changed.",
            ));
        }
        let record = self
            .store
            .stored_game(&request.dataset_id, &request.source, &request.game_id)?
            .ok_or_else(|| {
                AppError::new("source_unavailable", "The selected game is unavailable.")
            })?;
        let side = learner_side(&record, &request.learner.trim().to_lowercase())
            .ok_or_else(|| AppError::new("invalid_perspective", "Select one stored player."))?;
        let (record, played) =
            parse_stored_game(record).map_err(|message| AppError::new("invalid_game", message))?;
        let timeline = played.timeline();
        let mut introductions = BTreeMap::new();
        let mut book = None;
        let mut revision = None;
        let mut document_id = None;
        let mut comparison = None;
        if let Some(id) = &request.repertoire_id {
            let doc = self.store.repertoire_document(id)?;
            if doc.dataset != request.dataset_id {
                return Err(AppError::new(
                    "stale_dataset",
                    "The selected dataset changed.",
                ));
            }
            if doc.suspended {
                return Err(super::repertoires::suspended_repertoire());
            }
            let item = doc.item.ok_or_else(|| {
                AppError::new("unknown_repertoire", "Select a current repertoire.")
            })?;
            let book_side = item
                .definition
                .learner_color()
                .map_err(|e| domain("invalid_content", e))?;
            if side != book_side {
                return Err(AppError::new(
                    "invalid_perspective",
                    "Select a repertoire for the played side.",
                ));
            }
            revision = Some(doc.revision);
            if let Some(pgn) = doc.pgn {
                document_id = doc.source_id;
                let authored =
                    AuthoredRepertoire::parse(&pgn).map_err(|e| domain("invalid_content", e))?;
                comparison = Some(authored.compare(id, &played, side).0);
                let positions = timeline
                    .occurrences()
                    .iter()
                    .filter(|p| {
                        p.side_to_move() == side && authored.positions.contains_key(p.position())
                    })
                    .map(|p| p.position().clone())
                    .collect::<Vec<_>>();
                introductions = self.store.repertoire_introductions(
                    &doc.dataset,
                    doc.revision,
                    id,
                    &positions,
                )?;
                book = Some(authored);
            } else {
                comparison = Some(RepertoireAdherence {
                    repertoire_id: id.clone(),
                    source_available: false,
                    first_departure: None,
                    reentry_ply: None,
                });
            }
        }
        let occurrences = timeline.occurrences();
        let (start, played_moves) = played.mainline_identity();
        let start = PositionKey(start.to_owned());
        let mut positions = Vec::with_capacity(occurrences.len());
        for occurrence in occurrences {
            let key = occurrence.position();
            let (state, prepared) = preparation(book.as_ref(), key);
            let alternate = book.as_ref().and_then(|authored| {
                other_route(authored, &start, key, &played_moves[..occurrence.offset()])
            });
            positions.push(json!({
                "fen": occurrence.fen(),
                "played_uci": occurrence.next_move().map(|m| &m.uci),
                "prepared_uci": prepared,
                "book_state": state,
                "introduced": introductions.get(key),
                "other_route": alternate,
            }));
        }
        let mut line = Vec::new();
        if let Some(selection) = request.line {
            let anchor = occurrences.get(selection.from_ply).ok_or_else(|| {
                AppError::new(
                    "invalid_position",
                    "The selected board is outside this game.",
                )
            })?;
            let mut fen = anchor.fen().to_owned();
            for uci in selection.moves {
                let position = StandardPositionCanonicalizer
                    .canonicalize(&fen)
                    .map_err(|error| domain("invalid_position", error))?;
                let (state, prepared) = preparation(book.as_ref(), &position);
                let next =
                    apply_setup_move(&fen, &uci).map_err(|error| domain("invalid_move", error))?;
                line.push(
                    json!({"fen": fen, "uci": uci, "prepared_uci": prepared, "book_state": state}),
                );
                fen = next;
            }
        }
        Ok(json!({
            "status":"review", "dataset_id":request.dataset_id, "source":request.source,
            "game_id":request.game_id, "learner":request.learner, "white":record.white,
            "black":record.black, "result":played.result, "learner_color":side,
            "repertoire_id":request.repertoire_id, "revision":revision,
            "comparison":comparison, "document_id":document_id, "positions":positions, "line":line,
        }))
    }
}
