//! Recover previously committed allowance replies from the retired command.
use super::{Tempo, receipts::replay_accepted_receipt};
use crate::{
    AppError,
    app_error::domain,
    views::{EmptyView, StudyView},
    wire::STUDY_WIRE_VERSION,
};
use tempo_store::ReceiptResult;
use tempo_types::{DatasetId, SubmissionId};

impl Tempo {
    pub(super) fn replay_retired_allowance(
        &self,
        dataset: DatasetId,
        submission: SubmissionId,
    ) -> Result<StudyView, AppError> {
        let fingerprint = serde_json::to_string(&("learn_more_tactics", &dataset))
            .map_err(|error| domain("codec_error", error))?;
        let reply = match replay_accepted_receipt(
            self.store.as_ref(),
            &dataset,
            &submission,
            &fingerprint,
        )? {
            Some(ReceiptResult::TacticsAllowance { reply }) => reply,
            Some(_) => return Err(AppError::new("submission_conflict", "receipt kind differs")),
            None => return Err(retired_optional_tactics()),
        };
        let mut saved: EmptyView =
            serde_json::from_str(&reply).map_err(|error| domain("invalid_receipt", error))?;
        if saved.status != "empty"
            || saved.dataset_id != dataset
            || !matches!(saved.schema_version, 4 | STUDY_WIRE_VERSION)
        {
            return Err(AppError::new(
                "invalid_receipt",
                "Unrecognized saved tactics allowance response.",
            ));
        }
        // Versions 4 and 5 share the accepted empty-view fields.
        saved.schema_version = STUDY_WIRE_VERSION;
        Ok(StudyView::Empty(saved))
    }
}

pub(super) fn retired_optional_tactics() -> AppError {
    AppError::new(
        "retired_activity",
        "Optional Study tactics have been retired.",
    )
}
