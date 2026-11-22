//! Evaluate replies for a coverage continuation.
use crate::AppError;
use crate::gap_recommendation::{Recommendation, SimilarMoves, rank_and_recommend};
use crate::stockfish::{EngineResult, stockfish_until};
use std::{path::Path, time::Instant};

pub(crate) const NEARBY_PER_GROUP: usize = 8;
const MAX_COMPARISON_REPLIES: usize = 16;

pub(crate) fn evaluate_replies(
    similar: &mut SimilarMoves<'_>,
    directory: &Path,
    fen: &str,
    legal: &[String],
    deadline: Option<Instant>,
) -> (Result<EngineResult, AppError>, Option<Recommendation>) {
    let shortlist = similar
        .ordered()
        .take(MAX_COMPARISON_REPLIES)
        .map(|(_, mv)| mv.uci)
        .collect::<Vec<_>>();
    let engine = stockfish_until(directory, fen, legal, &shortlist, 3, deadline);
    let recommendation = engine.as_ref().ok().and_then(|result| {
        let best = result
            .comparison
            .as_ref()
            .unwrap_or(&result.candidates)
            .first()?;
        let scores = result
            .comparison
            .iter()
            .flatten()
            .map(|mv| (mv.uci.as_str(), mv.centipawns));
        rank_and_recommend(similar, (&best.uci, best.centipawns), scores)
    });
    (engine, recommendation)
}
