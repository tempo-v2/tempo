use super::Tempo;
use crate::{AppError, app_error::domain, game_backfill::BackfillJob};
use tempo_types::DatasetId;

impl Tempo {
    pub(crate) fn read_backfill(
        &self,
        dataset: &DatasetId,
        id: &str,
    ) -> Result<Option<BackfillJob>, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.game_backfill.read_backfill");
        self.store
            .game_backfill_state(dataset, Some(id))?
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| domain("invalid_backfill", error))
    }
}
