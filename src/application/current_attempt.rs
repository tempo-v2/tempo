//! Check that a new command addresses the current dataset and content version.
use super::Tempo;
use crate::AppError;
use tempo_store::{ResumeTarget, StoredAttempt, StudyState};
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
        check_attempt_dataset(record, dataset)?;
        let target = snapshot
            .target(&record.attempt.target)?
            .map(|(item, _)| ResumeTarget {
                version: item.version,
                status: item.status,
                excluded: snapshot.excluded_tactics.contains(&item.target),
                suspended: super::repertoires::is_suspended(item, &snapshot.suspended_repertoires),
            });
        check_current_target(
            record,
            id,
            snapshot.active_attempt.as_ref(),
            target.as_ref(),
        )?;
        Ok(record)
    }
}

pub(super) fn check_attempt_dataset(
    record: &StoredAttempt,
    dataset: &DatasetId,
) -> Result<(), AppError> {
    if &record.attempt.dataset != dataset {
        return Err(AppError::new(
            "stale_dataset",
            "attempt belongs to a different dataset",
        ));
    }
    Ok(())
}

pub(super) fn check_current_target(
    record: &StoredAttempt,
    id: &AttemptId,
    active: Option<&AttemptId>,
    target: Option<&ResumeTarget>,
) -> Result<(), AppError> {
    let target = target.ok_or_else(|| AppError::new("stale_content", "target is unavailable"))?;
    if target.excluded {
        return Err(AppError::new(
            "stale_activity",
            "This tactic was removed from Study.",
        ));
    }
    if target.suspended {
        return Err(super::repertoires::suspended_repertoire());
    }
    if target.status != tempo_chess::ContentStatus::Active
        || target.version != record.attempt.content_version
    {
        return Err(AppError::new(
            "stale_content",
            "exercise changed after this attempt was issued",
        ));
    }
    if record.attempt.status == tempo_study::AttemptStatus::Active && active != Some(id) {
        return Err(AppError::new(
            "stale_activity",
            "activity is no longer current",
        ));
    }
    Ok(())
}
