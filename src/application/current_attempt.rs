//! Check that a new command addresses the current dataset and content version.
use super::Tempo;
use crate::AppError;
use tempo_store::{StoredAttempt, StudyState};
use tempo_types::{AttemptId, DatasetId};

impl Tempo {
    // Receipt replay must run before this validation. A historical committed
    // reply remains replayable even after its target has changed.
    pub(super) fn current<'a>(
        &self,
        snapshot: &'a StudyState,
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
        let (item, _) = snapshot
            .target(&record.attempt.target)?
            .ok_or_else(|| AppError::new("stale_content", "target is unavailable"))?;
        if super::repertoires::is_suspended(item, &snapshot.suspended_repertoires) {
            return Err(super::repertoires::suspended_repertoire());
        }
        if !item.active() || item.version != record.attempt.content_version {
            return Err(AppError::new(
                "stale_content",
                "exercise changed after this attempt was issued",
            ));
        }
        if record.attempt.status == tempo_study::AttemptStatus::Active
            && snapshot.active_attempt.as_ref() != Some(id)
        {
            return Err(AppError::new(
                "stale_activity",
                "activity is no longer current",
            ));
        }
        Ok(record)
    }
}
