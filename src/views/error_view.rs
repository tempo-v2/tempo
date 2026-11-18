//! Application failure data that a transport can serialize as a normal reply.
use crate::{AppError, wire::CONTENT_WIRE_VERSION};
use serde::{Deserialize, Serialize};

/// Display a failure and tell the controller whether to retain its command.
///
/// The application maps [`crate::AppError`] into this view. `retryable` is a
/// recovery instruction for the same intent, not a grading or learning outcome.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorView {
    pub schema_version: u32,
    pub status: String,
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl ErrorView {
    /// Project failures from content commands using the content reply version.
    pub fn for_content(error: AppError) -> Self {
        Self::new(error, CONTENT_WIRE_VERSION)
    }

    pub(super) fn new(error: AppError, schema_version: u32) -> Self {
        Self {
            schema_version,
            status: "error".into(),
            code: error.code,
            message: error.message,
            retryable: error.retryable,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{AppError, views::StudyView};
    use tempo_store::StoreError;

    #[test]
    fn a_store_conflict_tells_the_controller_to_keep_the_same_command_for_retry() {
        let error = AppError::from(StoreError::Conflict("revision advanced".into()));
        let StudyView::Error(view) = StudyView::from(error) else {
            panic!("application errors map to the error variant");
        };
        let json = serde_json::to_value(view).unwrap();
        assert_eq!(json["status"], "error");
        assert_eq!(json["code"], "store_conflict");
        assert_eq!(json["retryable"], true);
        assert!(json.get("assessment").is_none());
    }
}
