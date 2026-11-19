//! Optional tactics continue after the required daily work is complete.
use super::{Tempo, receipts::replay};
use crate::{
    AppError,
    app_error::domain,
    views::{EmptyView, StudyView},
    wire::STUDY_WIRE_VERSION,
};
use tempo_store::{Receipt, ReceiptResult, StudyScope};
use tempo_types::{DatasetId, SubmissionId};

impl Tempo {
    pub(super) fn practice_tactics(&mut self) -> Result<StudyView, AppError> {
        let snapshot = self.store.study_state(StudyScope {
            catalog: true,
            ..StudyScope::default()
        })?;
        if let Some(id) = &snapshot.active_attempt {
            return self.project(self.resume_from(&snapshot, id)?);
        }
        self.daily.as_ref().ok_or_else(unavailable)?;
        let at = self.components.clock.now();
        let extra = self.store.tactics_extra(&snapshot.dataset, at.day)?;
        let selection = self.daily_selection(&snapshot, at, extra)?;
        if selection.outstanding != 0 {
            return Err(AppError::new(
                "daily_work_remaining",
                "Finish or resume the required work before optional tactics.",
            ));
        }
        let choice = selection.practice.ok_or_else(unavailable)?;
        let response = self.issue_daily(&snapshot, at, choice)?;
        self.project(response)
    }

    pub(super) fn learn_more_tactics(
        &mut self,
        dataset: DatasetId,
        submission: SubmissionId,
    ) -> Result<StudyView, AppError> {
        let scope = StudyScope {
            submission: Some(&submission),
            ..StudyScope::default()
        };
        let prior = self.store.study_state(scope)?;
        if prior.dataset != dataset {
            return Err(AppError::new(
                "stale_dataset",
                "This command belongs to a different dataset. Reload Tempo.",
            ));
        }
        let fingerprint = serde_json::to_string(&("learn_more_tactics", &dataset))
            .map_err(|e| domain("codec_error", e))?;
        if let Some(receipt) = replay(&prior, &submission, &fingerprint)? {
            return match receipt {
                ReceiptResult::TacticsAllowance { reply } => {
                    let mut saved: EmptyView =
                        serde_json::from_str(&reply).map_err(|e| domain("invalid_receipt", e))?;
                    if saved.status != "empty"
                        || saved.dataset_id != dataset
                        || !matches!(saved.schema_version, 4 | STUDY_WIRE_VERSION)
                    {
                        return Err(AppError::new(
                            "invalid_receipt",
                            "Unrecognized saved tactics allowance response.",
                        ));
                    }
                    // Wire 5 changed attempt display names. The Empty fields
                    // still describe the accepted allowance on its original day.
                    saved.schema_version = STUDY_WIRE_VERSION;
                    Ok(StudyView::Empty(saved))
                }
                _ => Err(AppError::new("submission_conflict", "receipt kind differs")),
            };
        }
        let increase = self.daily.as_ref().ok_or_else(unavailable)?.more_tactics;
        if increase == 0 {
            return Err(unavailable());
        }
        let snapshot = self.store.study_state(StudyScope {
            catalog: true,
            ..StudyScope::default()
        })?;
        if snapshot.revision != prior.revision || snapshot.dataset != dataset {
            return Err(tempo_store::StoreError::Conflict(
                "study changed; retry the same request".into(),
            )
            .into());
        }
        let at = self.components.clock.now();
        let extra = self.store.tactics_extra(&dataset, at.day)?;
        let selection = self.daily_selection(&snapshot, at, extra)?;
        if snapshot.active_attempt.is_some() || selection.outstanding != 0 {
            return Err(AppError::new(
                "daily_work_remaining",
                "Finish today's required work before increasing the allowance.",
            ));
        }
        if !selection.unseen_tactics {
            return Err(unavailable());
        }
        let extended = self.daily_selection(
            &snapshot,
            at,
            extra.checked_add(increase).ok_or_else(|| {
                AppError::new(
                    "allowance_exhausted",
                    "The extra allowance cannot be increased.",
                )
            })?,
        )?;
        let response = self.daily_summary(dataset.clone(), extended);
        let reply = serde_json::to_string(&response).map_err(|e| domain("codec_error", e))?;
        self.store.extend_tactics_allowance(
            &dataset,
            snapshot.revision,
            at.day,
            increase,
            &Receipt {
                id: submission,
                command: fingerprint,
                result: ReceiptResult::TacticsAllowance { reply },
            },
        )?;
        Ok(response)
    }
}

fn unavailable() -> AppError {
    AppError::new(
        "no_optional_tactics",
        "There are no tactics available for this action.",
    )
}
