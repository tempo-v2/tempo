//! Replay immutable results before applying current-state checks to new work.
use crate::AppError;
use tempo_store::{AnswerResult, ReceiptResult, StudyState};
use tempo_types::SubmissionId;

pub(super) fn replay(
    snapshot: &StudyState,
    id: &SubmissionId,
    command: &str,
) -> Result<Option<ReceiptResult>, AppError> {
    if id.0.is_empty() {
        return Err(AppError::new(
            "invalid_submission",
            "submission identity is required",
        ));
    }
    let Some(receipt) = snapshot.receipts.get(id) else {
        return Ok(None);
    };
    if receipt.command != command {
        return Err(AppError::new(
            "submission_conflict",
            "submission identity was reused with different meaning",
        ));
    }
    Ok(Some(receipt.result.clone()))
}
pub(super) fn study_result(result: ReceiptResult) -> Result<AnswerResult, AppError> {
    match result {
        ReceiptResult::Study(value) => Ok(value),
        _ => Err(AppError::new("submission_conflict", "receipt kind differs")),
    }
}
