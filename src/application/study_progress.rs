//! Read-only daily progress, independently of issuance and saved command replies.
use super::Tempo;
use crate::AppError;
use serde::{Deserialize, Serialize};
use tempo_store::StudyScope;
use tempo_types::{DatasetId, Revision, StudyDay};

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressStamp {
    pub dataset_id: DatasetId,
    pub day: StudyDay,
    pub revision: Revision,
}

#[derive(Debug, Serialize)]
pub struct StudyProgress {
    pub stamp: ProgressStamp,
    pub due: usize,
    pub reteach: usize,
    pub active_correction: bool,
    pub openings_introduced: usize,
    pub openings_limit: usize,
    pub tactics_introduced: usize,
    pub tactics_limit: usize,
    pub extra_tactics: usize,
}

impl Tempo {
    /// Preview today's progress for the UI without changing the active activity.
    /// An unchanged dataset/day/revision lets rollover checks skip catalog reads.
    pub fn study_progress(
        &self,
        known: Option<&ProgressStamp>,
    ) -> Result<Option<StudyProgress>, AppError> {
        let daily = self
            .daily
            .as_ref()
            .ok_or_else(|| AppError::new("unsupported", "Use the local study release."))?;
        let at = self.components.clock.now();
        if let Some(known) = known {
            let current = self.store.study_state(StudyScope::default())?;
            if known.day == at.day
                && known.dataset_id == current.dataset
                && known.revision == current.revision
            {
                return Ok(None);
            }
        }
        let snapshot = self.store.study_state(StudyScope {
            catalog: true,
            ..StudyScope::default()
        })?;
        let extra = self.store.tactics_extra(&snapshot.dataset, at.day)?;
        let selection = self.daily_selection(&snapshot, at, extra)?;
        let active_correction = snapshot
            .active_attempt
            .as_ref()
            .and_then(|id| snapshot.attempts.get(id))
            .is_some_and(|record| {
                record.mode == tempo_store::ActivityMode::Test
                    && record.attempt.terminal == Some(tempo_types::Grade::Again)
            });
        Ok(Some(StudyProgress {
            stamp: ProgressStamp {
                day: at.day,
                dataset_id: snapshot.dataset,
                revision: snapshot.revision,
            },
            due: selection.due_count,
            reteach: selection.reteach_count,
            active_correction,
            openings_introduced: selection.repertoire_admitted,
            openings_limit: daily.limits.repertoire,
            tactics_introduced: selection.tactics_admitted,
            tactics_limit: daily.limits.tactics.saturating_add(extra),
            extra_tactics: extra,
        }))
    }
}
