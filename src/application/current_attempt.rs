//! Check that a new command addresses the current dataset and content version.
use super::Tempo;
use crate::AppError;
use tempo_store::{Snapshot, StoredAttempt};
use tempo_types::{AttemptId, DatasetId};

impl Tempo {
    // Receipt replay must run before this validation. A historical committed
    // reply remains replayable even after its target has changed.
    pub(super) fn current<'a>(
        &self,
        snapshot: &'a Snapshot,
        dataset: &DatasetId,
        id: &AttemptId,
    ) -> Result<&'a StoredAttempt, AppError> {
        if dataset != &snapshot.dataset {
            return Err(AppError::new(
                "stale_dataset",
                "command belongs to a different dataset",
            ));
        }
        let record = snapshot
            .attempts
            .get(id)
            .ok_or_else(|| AppError::new("unknown_attempt", "attempt does not exist"))?;
        if &record.attempt.dataset != dataset {
            return Err(AppError::new(
                "stale_dataset",
                "attempt belongs to a different dataset",
            ));
        }
        let item = snapshot
            .catalog
            .items
            .get(&record.attempt.target)
            .ok_or_else(|| AppError::new("stale_content", "target is unavailable"))?;
        if !item.active() || item.version != record.attempt.content_version {
            return Err(AppError::new(
                "stale_content",
                "exercise changed after this attempt was issued",
            ));
        }
        Ok(record)
    }
}
