//! Replay immutable results before applying current-state checks to new work.
use crate::AppError;
use tempo_store::{
    AnswerResult, Receipt, ReceiptResult, StudyReceiptLookup, StudyReceiptRead, StudyState,
};
use tempo_types::{DatasetId, SubmissionId};

/// Check current dataset and the addressed immutable receipt before loading
/// attempt, Learning, content, or disposable projection state for new work.
pub(super) fn replay_accepted_study(
    store: &(impl StudyReceiptRead + ?Sized),
    dataset: &DatasetId,
    id: &SubmissionId,
    command: &str,
) -> Result<Option<AnswerResult>, AppError> {
    replay_accepted_receipt(store, dataset, id, command)?
        .map(study_result)
        .transpose()
}

pub(super) fn replay_accepted_receipt(
    store: &(impl StudyReceiptRead + ?Sized),
    dataset: &DatasetId,
    id: &SubmissionId,
    command: &str,
) -> Result<Option<ReceiptResult>, AppError> {
    validate_id(id)?;
    match store.study_receipt(dataset, id)? {
        StudyReceiptLookup::WrongDataset => Err(AppError::new(
            "stale_dataset",
            "command belongs to a different dataset",
        )),
        StudyReceiptLookup::Missing => Ok(None),
        StudyReceiptLookup::Found(receipt) => {
            check_command(&receipt, command)?;
            Ok(Some(receipt.result))
        }
    }
}

pub(super) fn replay_study_in_scope(
    snapshot: &StudyState,
    dataset: &DatasetId,
    id: &SubmissionId,
    command: &str,
) -> Result<Option<AnswerResult>, AppError> {
    if &snapshot.dataset != dataset {
        return Err(AppError::new(
            "stale_dataset",
            "command belongs to a different dataset",
        ));
    }
    replay(snapshot, id, command)?.map(study_result).transpose()
}

pub(super) fn replay(
    snapshot: &StudyState,
    id: &SubmissionId,
    command: &str,
) -> Result<Option<ReceiptResult>, AppError> {
    validate_id(id)?;
    let Some(receipt) = snapshot.receipts.get(id) else {
        return Ok(None);
    };
    check_command(receipt, command)?;
    Ok(Some(receipt.result.clone()))
}

fn validate_id(id: &SubmissionId) -> Result<(), AppError> {
    if id.0.is_empty() {
        return Err(AppError::new(
            "invalid_submission",
            "submission identity is required",
        ));
    }
    Ok(())
}

fn check_command(receipt: &Receipt, command: &str) -> Result<(), AppError> {
    if receipt.command != command {
        return Err(AppError::new(
            "submission_conflict",
            "submission identity was reused with different meaning",
        ));
    }
    Ok(())
}

pub(super) fn study_result(result: ReceiptResult) -> Result<AnswerResult, AppError> {
    match result {
        ReceiptResult::Study(value) => Ok(value),
        _ => Err(AppError::new("submission_conflict", "receipt kind differs")),
    }
}
