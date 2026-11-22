//! Address the saved scan starting line through the verified repertoire source.
use super::Tempo;
use crate::{AppError, app_error::domain};
use serde::{Deserialize, Serialize};
use tempo_chess::{AuthoredRepertoire, CoverageStart, LearnerColor};
use tempo_store::{CoverageLineSetting, ModelAuthority};
use tempo_types::{DatasetId, Revision};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CoverageLineRead {
    dataset_id: DatasetId,
    repertoire_id: String,
    candidate: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CoverageLineSave {
    dataset_id: DatasetId,
    repertoire_id: String,
    candidate: Vec<String>,
    save_at_revision: u64,
}

/// The previous installed client sent both operations to the read URL.
#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum PreviousCoverageLineRequest {
    Read(CoverageLineRead),
    Save(CoverageLineSave),
}

#[derive(Serialize)]
pub(crate) struct CoverageLineOptions {
    saved: CoverageLineSetting,
    line: Option<CoverageStart>,
    minimum_move: Option<u16>,
}

impl Tempo {
    pub(crate) fn saved_coverage_line(
        &self,
        dataset: &DatasetId,
        repertoire: &str,
    ) -> Result<CoverageLineSetting, AppError> {
        Ok(self.store.coverage_start(dataset, repertoire)?)
    }

    pub(crate) fn coverage_line_options(
        &self,
        request: CoverageLineRead,
    ) -> Result<CoverageLineOptions, AppError> {
        let (book, color, _) =
            self.coverage_settings_document(&request.dataset_id, &request.repertoire_id)?;
        let saved = self.saved_coverage_line(&request.dataset_id, &request.repertoire_id)?;
        let moves = request.candidate.as_deref().unwrap_or(&saved.moves);
        let (line, minimum_move) = inspect(&book, color, moves)?;
        Ok(CoverageLineOptions {
            saved,
            line,
            minimum_move,
        })
    }

    pub(crate) fn save_coverage_line(
        &mut self,
        request: CoverageLineSave,
    ) -> Result<CoverageLineOptions, AppError> {
        let (book, color, content_revision) =
            self.coverage_settings_document(&request.dataset_id, &request.repertoire_id)?;
        let (line, minimum_move) = inspect(&book, color, &request.candidate)?;
        if !request.candidate.is_empty() && !minimum_move.is_some_and(|number| number <= 20) {
            return Err(AppError::new(
                "invalid_scan",
                "Choose an authored line with a reply to check by move 20.",
            ));
        }
        let saved = self.store.save_coverage_start(
            &request.dataset_id,
            &request.repertoire_id,
            content_revision,
            request.save_at_revision,
            &request.candidate,
        )?;
        Ok(CoverageLineOptions {
            saved,
            line,
            minimum_move,
        })
    }

    fn coverage_settings_document(
        &self,
        dataset: &DatasetId,
        repertoire: &str,
    ) -> Result<(AuthoredRepertoire, LearnerColor, Revision), AppError> {
        let document = self.store.repertoire_document(repertoire)?;
        if document.dataset != *dataset {
            return Err(AppError::new(
                "stale_dataset",
                "The active dataset changed.",
            ));
        }
        if document.suspended {
            return Err(AppError::new(
                "coverage_unavailable",
                "Choose an active repertoire.",
            ));
        }
        let (ModelAuthority::Verified, Some(pgn), Some(item)) =
            (document.model_authority, document.pgn, document.item)
        else {
            return Err(AppError::new(
                "coverage_unavailable",
                "Choose a repertoire with a verified PGN.",
            ));
        };
        let book =
            AuthoredRepertoire::parse(&pgn).map_err(|error| domain("invalid_repertoire", error))?;
        let color = item
            .definition
            .learner_color()
            .map_err(|error| domain("invalid_repertoire", error))?;
        Ok((book, color, document.revision))
    }
}

fn inspect(
    book: &AuthoredRepertoire,
    color: LearnerColor,
    moves: &[String],
) -> Result<(Option<CoverageStart>, Option<u16>), AppError> {
    if moves.len() >= 40 {
        return Err(AppError::new(
            "invalid_scan",
            "Choose a saved opening line shorter than 20 moves.",
        ));
    }
    let line = book.coverage_start(moves, color).ok();
    let minimum_move = book
        .coverage_frontier(moves, color)
        .ok()
        .map(|(_, number)| number);
    Ok((line, minimum_move))
}
