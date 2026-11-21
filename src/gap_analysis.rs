//! On-demand authoring analysis. Neither result can write content or influence Study.
use crate::stockfish::{EngineResult, stockfish_until};
use crate::{
    AppError, Tempo,
    app_error::domain,
    game_evidence::check_id,
    game_model::GapRequest,
    gap_recommendation::{Recommendation, SimilarMoves, rank_and_recommend},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};
use tempo_chess::{GapDraft, PositionKey, SavedRepertoireResponse, nearby_repertoire_positions};
use tempo_types::Revision;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapAnalysisRequest {
    pub request_id: String,
    pub gap: GapRequest,
}

pub(crate) struct GapAnalysisInput {
    pub draft: GapDraft,
    pub catalog: BTreeMap<PositionKey, Vec<SavedRepertoireResponse>>,
    pub repertoire_id: String,
    pub repertoire_names: BTreeMap<String, String>,
    pub revision: Revision,
}

pub struct GapAnalysis {
    directory: PathBuf,
    reply: Arc<Mutex<Value>>,
}

impl GapAnalysis {
    pub fn new(database: &Path) -> Self {
        Self {
            directory: database.parent().unwrap_or(Path::new(".")).join("maia"),
            reply: Arc::new(Mutex::new(json!({"status":"idle"}))),
        }
    }

    pub fn status(&self) -> Value {
        self.reply.lock().expect("gap analysis reply").clone()
    }

    pub fn start(&self, app: &Tempo, request: GapAnalysisRequest) -> Result<Value, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("gap_analysis.dispatch");
        check_id(&request.request_id)?;
        if self.status()["status"] == "running" {
            return Err(AppError::new(
                "analysis_busy",
                "A position analysis is already running.",
            ));
        }
        let input = app.gap_analysis_input(request.gap)?;
        let queued = json!({"status":"running","request_id":request.request_id,"content_revision":input.revision});
        let reply = Arc::clone(&self.reply);
        let id = request.request_id;
        let directory = self.directory.clone();
        *reply.lock().expect("gap analysis reply") = queued.clone();
        let context = crate::diagnostics::capture_context();
        if let Err(error) = std::thread::Builder::new()
            .name("tempo-gap-analysis".into())
            .spawn(move || {
                crate::diagnostics::with_context(context, || {
                    let _worker = crate::diagnostics::Span::new("gap_analysis.worker");
                    let result = analyze(id, input, &directory, |progress| {
                        *reply.lock().expect("gap analysis reply") = progress;
                    });
                    *reply.lock().expect("gap analysis reply") = result;
                })
            })
        {
            *self.reply.lock().expect("gap analysis reply") = json!({"status":"idle"});
            return Err(domain("analysis_unavailable", error));
        }
        Ok(queued)
    }
}

pub(crate) const NEARBY_PER_GROUP: usize = 8;
const MAX_COMPARISON_REPLIES: usize = 16;

fn analyze(
    id: String,
    input: GapAnalysisInput,
    directory: &Path,
    publish: impl FnOnce(Value),
) -> Value {
    let _diagnostic = crate::diagnostics::Span::new("gap_analysis.execute");
    let nearby = nearby_repertoire_positions(
        &input.draft.fen,
        &input.repertoire_id,
        &input.catalog,
        NEARBY_PER_GROUP,
    );
    let mut similar = nearby.as_ref().map(SimilarMoves::rank).unwrap_or_default();
    // Older open pages can continue reading the original three board examples.
    let neighbors = nearby.as_ref().ok().map(|groups| {
        let mut positions = BTreeMap::<_, BTreeSet<_>>::new();
        for position in groups.current.iter().chain(&groups.other) {
            positions
                .entry((position.distance, &position.fen))
                .or_default()
                .extend(
                    position
                        .responses
                        .iter()
                        .filter_map(|saved| input.repertoire_names.get(&saved.repertoire_id)),
                );
        }
        positions
            .into_iter()
            .take(3)
            .map(|((_, fen), names)| json!({"fen": fen, "repertoires": names}))
            .collect::<Vec<_>>()
    });
    let mut progress = json!({
        "status": "running", "request_id": id, "content_revision": input.revision,
        "nearby": nearby.as_ref().ok(), "neighbors": neighbors, "neighbors_error": nearby.as_ref().err().map(ToString::to_string),
        "similar_responses": similar, "repertoire_names": input.repertoire_names,
    });
    publish(progress.clone());

    let (mut engine, recommendation) = evaluate_replies(
        &mut similar,
        directory,
        &input.draft.fen,
        &input.draft.legal_moves,
        None,
    );
    progress["similar_responses"] = json!(similar);
    progress["recommendation"] = json!(recommendation);
    if let Ok(result) = &mut engine
        && let Some(comparison) = result.comparison.take()
    {
        result.candidates = comparison.into_iter().take(3).collect();
    }
    progress["stockfish"] = json!(engine.as_ref().ok());
    progress["stockfish_error"] = json!(engine.err().map(|error| error.message));
    progress["status"] = json!("complete");
    progress
}

/// Evaluate the same shortlist for the editor and for a coverage continuation.
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
