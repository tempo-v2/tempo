//! Read verified authored context and the model evidence applied to it.
use super::{Tempo, content_preview::source_id};
use crate::coverage_scan::{GapLineInput, GapLineRequest};
use crate::game_model::{AuthoringSurface, GapKind, GapRequest, model_input_stamp};
use crate::maia::{MODEL_KEY, PROFILE_LABEL, checked_distribution};
use crate::{AppError, app_error::domain, commands::RepertoireInput};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tempo_chess::{
    AuthoredRepertoire, ContentKind, GapDraft, LearnerColor, PositionKey, SavedRepertoireResponse,
    compile_repertoire, legal_move_labels,
};
use tempo_store::{
    CoverageEditBasis, ForecastGap, ModelAuthority, ProjectionStatus, StoreError, model_lines,
    model_targets,
};
use tempo_types::{DatasetId, Revision};

struct CheckedGapRepertoire {
    authored: AuthoredRepertoire,
    revision: Revision,
    name: String,
    color: LearnerColor,
    pgn: String,
}

impl Tempo {
    pub(crate) fn model_projection_status(
        &self,
        dataset: &DatasetId,
    ) -> Result<Option<ProjectionStatus>, AppError> {
        Ok(self.store.game_model_projection_status(dataset)?)
    }

    /// Verify legacy source associations once against accepted content before cache reprojection.
    pub fn bootstrap_model_inputs(&mut self) -> Result<(), AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.game_model.bootstrap_model_inputs");
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
                            crate::diagnostics::measure("chess.compile_repertoire", || {
                                compile_repertoire(pgn, &id, &item.label, color, &source_id)
                            })
                            .ok()?;
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

    pub(crate) fn coverage_summary(
        &self,
        dataset: &DatasetId,
        repertoire: &str,
    ) -> Result<Option<tempo_store::SavedCoverage>, AppError> {
        if &self.game_dataset()? != dataset {
            return Err(AppError::new(
                "stale_dataset",
                "The active dataset changed.",
            ));
        }
        Ok(self.store.coverage_summary(dataset, repertoire)?)
    }

    pub fn opponent_gaps(&mut self, offset: usize) -> Result<Value, AppError> {
        self.opponent_gap_page(offset, None)
    }

    pub(crate) fn opponent_gap_page(
        &mut self,
        offset: usize,
        forecast_book: Option<&str>,
    ) -> Result<Value, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.game_model.opponent_gaps");
        let dataset = self.game_dataset()?;
        let candidates = self.store.opponent_gaps(&dataset)?;
        let forecasts = self.store.forecast_gaps(&dataset, forecast_book)?;
        if candidates.is_empty() && forecasts.is_empty() {
            return Ok(json!({"dataset_id":dataset,"gaps":[]}));
        }
        let books = self.current_game_books(&dataset)?;
        let observed_counts: BTreeMap<_, _> = candidates
            .iter()
            .map(|group| (&group.gap, group.games))
            .collect();
        let mut ordered = Vec::new();
        let mut forecast_keys = BTreeSet::new();
        let mut valid_starts = BTreeMap::new();
        let limit = offset.saturating_add(101);
        // Store supplies forecasts in descending probability and observed
        // groups in descending count, both with stable identity ties.
        for forecast in &forecasts {
            if ordered.len() == limit {
                break;
            }
            let gap = &forecast.gap;
            let moves = &forecast.starting_line.moves;
            if forecast.stale
                && !moves.is_empty()
                && !*valid_starts
                    .entry(&gap.repertoire_id)
                    .or_insert_with(|| books.has_forecast_start(&gap.repertoire_id, moves))
            {
                continue;
            }
            if let Some(revision) = books.document_for_forecast(gap, forecast.stale) {
                forecast_keys.insert(gap);
                ordered.push((gap, revision, Some(forecast)));
            }
        }
        if forecast_book.is_none() {
            for group in &candidates {
                if ordered.len() == limit {
                    break;
                }
                if !forecast_keys.contains(&group.gap)
                    && let Some(revision) = books.document_for_gap(&group.gap)
                {
                    ordered.push((&group.gap, revision, None));
                }
            }
        }
        let page: Vec<_> = ordered
            .into_iter()
            .skip(offset)
            .take(101)
            .map(|(gap, revision, forecast)| {
                let games = observed_counts.get(gap).copied().unwrap_or(0);
                let kind = if forecast.is_some() {
                    "forecast"
                } else {
                    "observed"
                };
                let labels = legal_move_labels(&format!("{} 0 1", gap.position.0)).ok();
                let san = labels
                    .as_ref()
                    .and_then(|moves| moves.get(&gap.actual_move));
                json!({
                    "repertoire_id":gap.repertoire_id, "position":gap.position,
                    "actual_move":gap.actual_move, "actual_san":san, "games":games,
                    "document_revision":revision, "gap_kind":kind,
                    "forecast":forecast.map(|forecast| json!({
                        "probability":forecast.probability, "stale":forecast.stale,
                        "report_id":forecast.report_id, "through_move":forecast.through_move,
                    })),
                })
            })
            .collect();
        Ok(
            json!({"dataset_id":dataset,"gaps":page.iter().take(100).collect::<Vec<_>>(),"more":page.len()>100}),
        )
    }

    fn checked_gap_repertoire(
        &self,
        request: &GapRequest,
    ) -> Result<CheckedGapRepertoire, AppError> {
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
        if doc.suspended {
            return Err(super::repertoires::suspended_repertoire());
        }
        let color = item
            .definition
            .learner_color()
            .map_err(|error| domain("invalid_content", error))?;
        let authored =
            AuthoredRepertoire::parse(&pgn).map_err(|error| domain("invalid_repertoire", error))?;
        Ok(CheckedGapRepertoire {
            authored,
            revision: doc.revision,
            name: item.label,
            color,
            pgn,
        })
    }

    fn checked_gap(
        &self,
        request: &GapRequest,
    ) -> Result<(CheckedGapRepertoire, GapDraft, Option<CoverageEditBasis>), AppError> {
        let book = self.checked_gap_repertoire(request)?;
        let mut basis = None;
        let draft = match request.gap_kind {
            GapKind::Observed => book.authored.gap_draft(
                &book.pgn,
                &request.position,
                &request.actual_move,
                book.color,
                request.response.as_deref(),
            ),
            GapKind::Forecast => {
                let forecast = self.forecast_gap(request, request.report_id.as_deref())?;
                basis = Some(forecast.edit_basis());
                book.authored
                    .coverage_line_from(
                        &forecast.starting_line.moves,
                        &request.position,
                        &request.actual_move,
                        book.color,
                    )
                    .and_then(|line| line.into_gap_draft(&book.pgn, request.response.as_deref()))
            }
        }
        .map_err(|error| domain("stale_gap", error))?;
        Ok((book, draft, basis))
    }

    pub(crate) fn gap_position(
        &self,
        mut request: GapRequest,
    ) -> Result<(GapDraft, Revision), AppError> {
        request.response = None;
        let (book, draft, _) = self.checked_gap(&request)?;
        Ok((draft, book.revision))
    }

    fn gap_analysis_context(
        &self,
        dataset: &DatasetId,
        repertoire_id: &str,
        revision: Revision,
    ) -> Result<BTreeMap<PositionKey, Vec<SavedRepertoireResponse>>, AppError> {
        let snapshot = self.store.content_state()?;
        let current_revision = snapshot.content_revision;
        if &snapshot.dataset != dataset || current_revision != revision {
            return Err(AppError::new(
                "stale_repertoire",
                "The repertoire changed. Open the gap again.",
            ));
        }
        if snapshot.suspended_repertoires.contains(repertoire_id) {
            return Err(super::repertoires::suspended_repertoire());
        }
        let mut catalog = BTreeMap::<PositionKey, Vec<SavedRepertoireResponse>>::new();
        for item in snapshot.catalog.items.values().filter(|item| item.active()) {
            if let ContentKind::Repertoire {
                position,
                repertoire,
            } = &item.definition.kind
            {
                if snapshot.suspended_repertoires.contains(repertoire) {
                    continue;
                }
                catalog
                    .entry(position.clone())
                    .or_default()
                    .push(SavedRepertoireResponse {
                        repertoire_id: repertoire.clone(),
                        answer: item.definition.decisions[0].expected.clone(),
                    });
            }
        }
        Ok(catalog)
    }

    pub(crate) fn authorize_gap_surface(
        &self,
        request: &GapRequest,
        surface: AuthoringSurface,
    ) -> Result<(), AppError> {
        if surface == AuthoringSurface::PhoneBuild {
            self.phone_forecast_gap(request, request.report_id.as_deref())?;
        }
        Ok(())
    }

    fn phone_forecast_gap(
        &self,
        gap: &GapRequest,
        report: Option<&str>,
    ) -> Result<ForecastGap, AppError> {
        if gap.gap_kind != GapKind::Forecast {
            return Err(AppError::new(
                "phone_build_only",
                "Choose a missing reply from a repertoire coverage scan.",
            ));
        }
        self.forecast_gap(gap, report)
    }

    fn forecast_gap(
        &self,
        gap: &GapRequest,
        report: Option<&str>,
    ) -> Result<ForecastGap, AppError> {
        self.store
            .forecast_gaps(&gap.dataset_id, Some(&gap.repertoire_id))?
            .into_iter()
            .find(|entry| {
                report.is_none_or(|id| entry.report_id == id)
                    && entry.gap.position == gap.position
                    && entry.gap.actual_move == gap.actual_move
            })
            .ok_or_else(|| AppError::new("stale_coverage", "Refresh the gaps for the latest scan."))
    }

    pub(crate) fn gap_line_input(
        &self,
        request: &GapLineRequest,
        surface: AuthoringSurface,
    ) -> Result<GapLineInput, AppError> {
        let mut gap = request.gap.clone();
        let first = gap
            .response
            .take()
            .ok_or_else(|| AppError::new("invalid_gap_line", "Choose your first reply."))?;
        let checked = self.checked_gap_repertoire(&gap)?;
        if gap.base_revision != Some(checked.revision) {
            return Err(AppError::new(
                "stale_draft",
                "Reopen the gap before drafting a line.",
            ));
        }
        if gap
            .report_id
            .as_ref()
            .is_some_and(|id| id != &request.report_id)
        {
            return Err(AppError::new(
                "stale_coverage",
                "Reopen the gap from the latest scan.",
            ));
        }
        let report = Some(request.report_id.as_str());
        let forecast = match surface {
            AuthoringSurface::Desktop => self.forecast_gap(&gap, report),
            AuthoringSurface::PhoneBuild => self.phone_forecast_gap(&gap, report),
        }?;
        let mut line = checked
            .authored
            .coverage_line_from(
                &forecast.starting_line.moves,
                &gap.position,
                &gap.actual_move,
                checked.color,
            )
            .map_err(|error| domain("stale_gap", error))?;
        let anchor_fen = line.fen();
        let first_number = line.move_number();
        if first_number > u16::from(forecast.through_move) {
            return Err(AppError::new(
                "stale_coverage",
                "The gap is beyond the scan depth.",
            ));
        }
        let first_san = line
            .play(&checked.authored, &first)
            .map_err(|error| domain("invalid_gap_line", error))?;
        let catalog =
            self.gap_analysis_context(&gap.dataset_id, &gap.repertoire_id, checked.revision)?;
        Ok(GapLineInput {
            line,
            anchor_fen,
            first,
            first_san,
            first_number,
            catalog,
            book: checked.authored,
            source: checked.pgn,
            name: checked.name,
            color: checked.color,
            through_move: forecast.through_move,
            forecast: forecast.edit_basis(),
        })
    }

    pub(crate) fn sign_gap_line(
        &self,
        input: &RepertoireInput,
        revision: Revision,
        surface: AuthoringSurface,
        request: &GapLineRequest,
        basis: &CoverageEditBasis,
    ) -> Result<String, AppError> {
        let forecast = self.forecast_gap(&request.gap, Some(&request.report_id))?;
        if forecast.edit_basis() != *basis {
            return Err(AppError::new(
                "stale_coverage",
                "Reopen the gap from the latest scan.",
            ));
        }
        self.gap_response_token(
            input,
            input.repertoire_id.as_deref().expect("gap repertoire"),
            revision,
            surface,
            Some(basis),
        )
    }

    pub fn game_gap_draft(&self, request: GapRequest) -> Result<Value, AppError> {
        self.game_gap_draft_on(request, AuthoringSurface::Desktop)
    }

    pub(crate) fn game_gap_draft_on(
        &self,
        request: GapRequest,
        surface: AuthoringSurface,
    ) -> Result<Value, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.game_model.game_gap_draft");
        self.authorize_gap_surface(&request, surface)?;
        let (
            CheckedGapRepertoire {
                authored,
                revision,
                name,
                color,
                ..
            },
            draft,
            forecast,
        ) = self.checked_gap(&request)?;
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
                    surface,
                    forecast.as_ref(),
                )
            })
            .transpose()?;
        let before = legal_move_labels(&format!("{} 0 1", request.position.0))
            .map_err(|error| domain("invalid_gap_position", error))?;
        let after =
            legal_move_labels(&draft.fen).map_err(|error| domain("invalid_gap_position", error))?;
        let checked = self
            .store
            .game_move_distribution(MODEL_KEY, &request.position.0)
            .map_err(AppError::from)
            .and_then(|cached| {
                cached
                    .map(|moves| checked_distribution(&request.position, moves))
                    .transpose()
            });
        let (distribution, local_model_error) = match checked {
            Ok(distribution) => (distribution, None),
            Err(error) => (None, Some(error.code)),
        };
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
            "anchor_route": draft.anchor_route,
            "content_revision": revision,
            "save_token": save_token,
            "repertoire_name": name,
            "learner_color": match color { LearnerColor::White => "white", LearnerColor::Black => "black" },
            "actual_san": before.get(&request.actual_move),
            "opponent_move_labels": before,
            "response_move_labels": after,
            "profile": PROFILE_LABEL,
            "local_model_error": local_model_error,
            "local_coverage": coverage,
            "actual_probability": actual_probability,
            "top_replies": top,
        }))
    }
}
