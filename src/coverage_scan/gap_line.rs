//! Preview one continuation using coverage evidence, then authorize the existing signed Save.
use super::{CoverageScan, ExplorerCache, Sources};
use crate::{
    AppError, Tempo,
    app_error::domain,
    commands::RepertoireInput,
    explorer::ExplorerClient,
    game_evidence::check_id,
    game_model::{AuthoringSurface, GapRequest},
    gap_analysis::{NEARBY_PER_GROUP, evaluate_replies},
    gap_recommendation::SimilarMoves,
    maia::Distribution,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tempo_chess::{
    AuthoredRepertoire, CoverageLine, LearnerColor, PositionKey, SavedRepertoireResponse,
    nearby_repertoire_positions,
};
use tempo_store::{CoverageEditBasis, SqliteStore};
use tempo_types::Revision;

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct GapLineRequest {
    pub request_id: String,
    pub report_id: String,
    pub gap: GapRequest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GapLineRead {
    request_id: String,
}

pub(crate) struct GapLineInput {
    pub line: CoverageLine,
    pub anchor_fen: String,
    pub first: String,
    pub first_san: String,
    pub first_number: u16,
    pub catalog: BTreeMap<PositionKey, Vec<SavedRepertoireResponse>>,
    pub book: AuthoredRepertoire,
    pub source: String,
    pub name: String,
    pub color: LearnerColor,
    pub through_move: u8,
    pub forecast: CoverageEditBasis,
}

#[derive(Default)]
pub(super) struct LineJobs {
    active: bool,
    latest: Option<LineRun>,
    waiting: Option<(GapLineRequest, GapLineInput)>,
}

struct LineRun {
    request: GapLineRequest,
    surface: AuthoringSurface,
    through_move: u8,
    forecast: CoverageEditBasis,
    reached: Option<u16>,
    result: Option<Result<Arc<Preview>, AppError>>,
}

struct Preview {
    anchor_fen: String,
    anchor_route: Vec<String>,
    steps: Vec<Step>,
    reason: Stop,
    save: RepertoireInput,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Stop {
    Depth,
    Terminal,
    OpponentEvidence,
    LearnerEvidence,
    Deadline,
}

#[derive(Serialize)]
struct Step {
    uci: String,
    san: String,
    move_number: u16,
    learner: bool,
    new_reply: bool,
    probability: Option<f64>,
}

impl CoverageScan {
    pub(crate) fn start_line(
        &self,
        app: &Tempo,
        request: GapLineRequest,
        surface: AuthoringSurface,
    ) -> Result<Value, AppError> {
        check_id(&request.request_id)?;
        let read = GapLineRead {
            request_id: request.request_id.clone(),
        };
        let jobs = self.lines.lock().expect("gap line preview");
        if let Some(run) = &jobs.latest
            && run.request.request_id == request.request_id
        {
            if run.request != request || run.surface != surface {
                return Err(AppError::new(
                    "invalid_gap_line",
                    "This line request ID is already in use.",
                ));
            }
            drop(jobs);
            return self.read_line(app, read, surface);
        }

        drop(jobs);
        let input = app.gap_line_input(&request, surface)?;
        let mut jobs = self.lines.lock().expect("gap line preview");
        jobs.latest = Some(LineRun {
            request: request.clone(),
            surface,
            through_move: input.through_move,
            forecast: input.forecast.clone(),
            reached: None,
            result: None,
        });
        jobs.waiting = Some((request, input));
        if !jobs.active {
            jobs.active = true;
            let state = Arc::clone(&self.lines);
            let database = self.database.clone();
            let explorer = Arc::clone(&self.explorer);
            let cache = Arc::clone(&self.cache);
            if let Err(error) = std::thread::Builder::new()
                .name("tempo-gap-line".into())
                .spawn(move || run_jobs(&state, &database, &explorer, &cache))
            {
                *jobs = LineJobs::default();
                return Err(domain("coverage_unavailable", error));
            }
        }
        drop(jobs);
        self.read_line(app, read, surface)
    }

    pub(crate) fn read_line(
        &self,
        app: &Tempo,
        read: GapLineRead,
        surface: AuthoringSurface,
    ) -> Result<Value, AppError> {
        let jobs = self.lines.lock().expect("gap line preview");
        let run = jobs
            .latest
            .as_ref()
            .filter(|run| run.request.request_id == read.request_id && run.surface == surface)
            .ok_or_else(|| AppError::new("unknown_request", "Request this line again."))?;
        let revision = run
            .request
            .gap
            .base_revision
            .expect("validated line revision");
        let request = run.request.clone();
        let forecast = run.forecast.clone();
        let through_move = run.through_move;
        let reached = run.reached;
        let result = run.result.clone();
        drop(jobs);
        let mut reply = json!({
            "request_id": read.request_id, "content_revision": revision,
            "through_move": through_move, "reached": reached, "status": "running"
        });
        if let Some(result) = result {
            let preview = result?;
            reply["status"] = json!("ready");
            reply["anchor_fen"] = json!(preview.anchor_fen);
            reply["anchor_route"] = json!(preview.anchor_route);
            reply["steps"] = json!(preview.steps);
            reply["stop"] = json!(preview.reason);
            if preview.steps.iter().filter(|step| step.learner).count() > 1 {
                reply["save_command"] =
                    signed_save(app, &preview.save, revision, surface, &request, &forecast)?;
            }
        }
        Ok(reply)
    }
}

fn signed_save(
    app: &Tempo,
    input: &RepertoireInput,
    revision: Revision,
    surface: AuthoringSurface,
    request: &GapLineRequest,
    forecast: &CoverageEditBasis,
) -> Result<Value, AppError> {
    let token = app.sign_gap_line(input, revision, surface, request, forecast)?;
    let mut save = json!(input);
    save["command"] = json!("save");
    save["token"] = json!(token);
    Ok(save)
}

fn run_jobs(
    state: &Mutex<LineJobs>,
    database: &Path,
    explorer: &ExplorerClient,
    cache: &ExplorerCache,
) {
    loop {
        let mut jobs = state.lock().expect("gap line preview");
        let Some((request, input)) = jobs.waiting.take() else {
            jobs.active = false;
            return;
        };
        drop(jobs);
        let update = |reached| {
            let mut jobs = state.lock().expect("gap line preview");
            let Some(run) = jobs.latest.as_mut().filter(|run| run.request == request) else {
                return false;
            };
            run.reached = Some(reached);
            true
        };
        let result = preview(&request, input, database, explorer, cache, update);
        let mut jobs = state.lock().expect("gap line preview");
        if let Some(run) = jobs.latest.as_mut().filter(|run| run.request == request) {
            run.result = Some(result.map(Arc::new));
        }
    }
}

fn preview(
    request: &GapLineRequest,
    input: GapLineInput,
    database: &Path,
    explorer: &ExplorerClient,
    cache: &ExplorerCache,
    progress: impl Fn(u16) -> bool,
) -> Result<Preview, AppError> {
    let store = SqliteStore::open_read_only(database)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut sources = Sources::new(deadline, &store, explorer, cache, database);
    let directory = sources.directory.clone();
    let mut accepted = input.line.clone();
    let mut line = input.line;
    let first = Step {
        uci: input.first,
        san: input.first_san,
        move_number: input.first_number,
        learner: true,
        new_reply: true,
        probability: None,
    };
    let (steps, reason) = extend_line(
        &mut line,
        &input.book,
        first,
        input.through_move,
        deadline,
        progress,
        |key, legal| sources.distribution(key, legal),
        |fen, legal| {
            let nearby = nearby_repertoire_positions(
                fen,
                &request.gap.repertoire_id,
                &input.catalog,
                NEARBY_PER_GROUP,
            );
            let mut similar = nearby.as_ref().map(SimilarMoves::rank).unwrap_or_default();
            let (engine, recommendation) =
                evaluate_replies(&mut similar, &directory, fen, legal, Some(deadline));
            recommendation.map(|entry| entry.into_move()).or_else(|| {
                let engine = engine.ok()?;
                engine
                    .comparison
                    .as_ref()
                    .unwrap_or(&engine.candidates)
                    .first()
                    .map(|candidate| candidate.uci.clone())
            })
        },
    )?;
    // Reconstruct only the accepted prefix when a predicted move had no reply.
    for step in steps.iter().skip(1) {
        accepted
            .play(&input.book, &step.uci)
            .map_err(|error| domain("invalid_gap_line", error))?;
    }
    Ok(Preview {
        anchor_fen: input.anchor_fen,
        anchor_route: accepted.anchor_route().to_vec(),
        steps,
        reason,
        save: RepertoireInput {
            dataset_id: request.gap.dataset_id.clone(),
            repertoire_id: Some(request.gap.repertoire_id.clone()),
            name: input.name,
            learner_color: input.color,
            pgn: accepted.append_to(&input.source),
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn extend_line(
    line: &mut CoverageLine,
    book: &AuthoredRepertoire,
    first: Step,
    through_move: u8,
    deadline: Instant,
    progress: impl Fn(u16) -> bool,
    mut opponent: impl FnMut(&PositionKey, &[String]) -> Distribution,
    mut learner: impl FnMut(&str, &[String]) -> Option<String>,
) -> Result<(Vec<Step>, Stop), AppError> {
    let depth = u16::from(through_move);
    let mut reached = first.move_number;
    let mut steps = vec![first];
    let reason = loop {
        if !progress(reached) {
            return Err(AppError::new(
                "unknown_request",
                "A newer line replaced this preview.",
            ));
        }
        if reached >= depth {
            break Stop::Depth;
        }
        if Instant::now() >= deadline {
            break Stop::Deadline;
        }
        let key = line.position();
        let legal = tempo_chess::legal_model_moves(&key)
            .map_err(|error| domain("invalid_gap_line", error))?;
        if legal.is_empty() {
            break Stop::Terminal;
        }
        let is_learner = line.learner_turn();
        let prepared = is_learner.then(|| line.prepared_reply(book)).flatten();
        let choice = if is_learner {
            prepared
                .map(str::to_owned)
                .or_else(|| learner(&line.fen(), &legal))
                .map(|uci| (uci, None))
        } else {
            opponent(&key, &legal)
                .into_iter()
                .filter(|(uci, p)| *p > 0.0 && legal.contains(uci))
                .min_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)))
                .map(|(uci, p)| (uci, Some(p)))
        };
        let Some((uci, probability)) = choice else {
            break if Instant::now() >= deadline {
                Stop::Deadline
            } else if is_learner {
                Stop::LearnerEvidence
            } else {
                Stop::OpponentEvidence
            };
        };
        let move_number = line.move_number();
        let new_reply = is_learner && prepared.is_none();
        let san = line
            .play(book, &uci)
            .map_err(|error| domain("invalid_gap_line", error))?;
        steps.push(Step {
            uci,
            san,
            move_number,
            learner: is_learner,
            new_reply,
            probability,
        });
        if is_learner {
            reached = move_number;
        }
    };
    if !matches!(reason, Stop::Terminal) && steps.last().is_some_and(|step| !step.learner) {
        steps.pop();
    }
    Ok((steps, reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn likely_line_counts_learner_move_numbers_for_both_colors() {
        use tempo_chess::{PositionCanonicalizer, START_FEN, StandardPositionCanonicalizer};

        for (pgn, color, anchor, gap, first, first_number) in [
            (
                "1.e4 e5 2.Nf3 *",
                LearnerColor::White,
                "1.e4",
                "d7d5",
                "e4d5",
                2,
            ),
            ("1.e4 e5 *", LearnerColor::Black, "", "d2d4", "d7d5", 1),
        ] {
            let book = AuthoredRepertoire::parse(pgn).unwrap();
            let anchor = if anchor.is_empty() {
                StandardPositionCanonicalizer
                    .canonicalize(START_FEN)
                    .unwrap()
            } else {
                AuthoredRepertoire::parse("1.e4 *")
                    .unwrap()
                    .positions
                    .into_keys()
                    .find(|key| key.0.contains(" b "))
                    .unwrap()
            };
            for depth in [3, 8] {
                let mut line = book.coverage_line(&anchor, gap, color).unwrap();
                assert_eq!(line.move_number(), first_number);
                let san = line.play(&book, first).unwrap();
                let first = Step {
                    uci: first.into(),
                    san,
                    move_number: first_number,
                    learner: true,
                    new_reply: true,
                    probability: None,
                };
                let (steps, reason) = extend_line(
                    &mut line,
                    &book,
                    first,
                    depth,
                    Instant::now() + Duration::from_secs(2),
                    |_| true,
                    |_, legal| {
                        legal
                            .iter()
                            .enumerate()
                            .map(|(index, uci)| {
                                (
                                    uci.clone(),
                                    if index + 1 == legal.len() { 0.9 } else { 0.001 },
                                )
                            })
                            .collect()
                    },
                    |_, legal| legal.first().cloned(),
                )
                .unwrap();
                assert!(matches!(reason, Stop::Depth));
                assert_eq!(steps.last().unwrap().move_number, u16::from(depth));
                assert!(steps.last().unwrap().learner);
                assert!(
                    steps
                        .iter()
                        .filter_map(|step| step.probability)
                        .all(|p| p == 0.9)
                );
                assert_eq!(
                    steps.iter().filter(|step| step.learner).count(),
                    usize::from(u16::from(depth) - first_number + 1)
                );
            }
        }
    }
    #[test]
    fn continuation_reuses_the_transposed_answer_and_stops_at_the_last_complete_reply() {
        let book = AuthoredRepertoire::parse("1.Nf3 Nf6 2.g3 g6 3.Bg2 *").unwrap();
        let anchor = book
            .positions
            .iter()
            .find(|(_, node)| node.prefix == ["g1f3"])
            .unwrap()
            .0;
        let mut line = book
            .coverage_line(anchor, "g7g6", LearnerColor::White)
            .unwrap();
        let san = line.play(&book, "g2g3").unwrap();
        let first = Step {
            uci: "g2g3".into(),
            san,
            move_number: 2,
            learner: true,
            new_reply: true,
            probability: None,
        };
        let mut calls = 0;
        let (steps, reason) = extend_line(
            &mut line,
            &book,
            first,
            8,
            Instant::now() + Duration::from_secs(2),
            |_| true,
            |_, _| {
                calls += 1;
                if calls == 1 {
                    Distribution::from([("g8f6".into(), 1.0)])
                } else {
                    Distribution::new()
                }
            },
            |_, _| panic!("the transposed position already has an answer"),
        )
        .unwrap();
        assert!(matches!(reason, Stop::OpponentEvidence));
        assert_eq!(
            steps
                .iter()
                .map(|step| step.uci.as_str())
                .collect::<Vec<_>>(),
            ["g2g3", "g8f6", "f1g2"]
        );
        assert!(!steps.last().unwrap().new_reply);
    }
}
