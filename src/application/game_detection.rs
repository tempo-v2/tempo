//! Read the requested accepted game and resolve its learner before dispatch.
use super::{Tempo, game_evidence::learner_side};
use crate::{
    AppError,
    game_detection::{AcceptedDetectionSource, DetectionRequest},
};

impl Tempo {
    pub(crate) fn accepted_detection_source(
        &self,
        request: &DetectionRequest,
    ) -> Result<AcceptedDetectionSource, AppError> {
        let unavailable =
            || AppError::new("source_unavailable", "The accepted game is unavailable.");
        if self.game_dataset()? != request.dataset_id {
            return Err(unavailable());
        }
        let record = self
            .store
            .stored_game(&request.dataset_id, &request.source, &request.game_id)?
            .ok_or_else(unavailable)?;
        let color = learner_side(&record, &request.learner.trim().to_lowercase())
            .ok_or_else(|| AppError::new("invalid_perspective", "Select one stored player."))?;
        Ok(AcceptedDetectionSource {
            pgn: record.pgn,
            color,
        })
    }
}
