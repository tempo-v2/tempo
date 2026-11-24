//! Read verified authored context and the model evidence applied to it.
use super::{Tempo, content_preview::source_id};
use crate::game_model::{
    AuthoringSurface, GapKind, GapRequest, MODEL_KEY, PROFILE_LABEL, checked_distribution,
};
use crate::{AppError, app_error::domain, commands::RepertoireInput};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tempo_chess::{AuthoredRepertoire, GapDraft, LearnerColor, legal_move_labels};
use tempo_store::{CoverageEditBasis, ForecastGap, ProjectionStatus};
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
