//! Application failures translated into stable codes for callers and wire views.
use std::{error::Error, fmt};
use tempo_store::StoreError;
use tempo_study::{GradingError, StudyError};

/// A failed application operation, with a code callers can branch on.
///
/// Reach for this at an application adapter boundary. Domain crates keep their
/// own error types; Tempo maps them here before returning a wire error view.
/// `retryable` means the caller may retry the same command and identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}
impl AppError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        crate::diagnostics::emit(
            "application.error",
            serde_json::json!({"level":"warn", "error_code":code}),
        );
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
        }
    }
}
impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl Error for AppError {}
impl From<StoreError> for AppError {
    fn from(value: StoreError) -> Self {
        let code = match &value {
            StoreError::Unavailable(_) => "store_unavailable",
            StoreError::Conflict(_) => "store_conflict",
            StoreError::Invalid(_) => "store_invalid",
            StoreError::Unsupported(_) => "unsupported_capability",
            StoreError::StaleCoverage => {
                return AppError::new(
                    "stale_coverage",
                    "The coverage forecast changed. Reopen the gap before saving.",
                );
            }
        };
        crate::diagnostics::emit(
            "application.error",
            serde_json::json!({"level":"warn", "error_code":code}),
        );
        Self {
            code: code.into(),
            message: value.to_string(),
            retryable: !matches!(value, StoreError::Invalid(_) | StoreError::Unsupported(_)),
        }
    }
}
pub(crate) fn domain(code: &str, error: impl fmt::Display) -> AppError {
    AppError::new(code, error.to_string())
}

pub(crate) fn study_input(error: StudyError) -> AppError {
    match error {
        StudyError::Grading(GradingError::InvalidAnswer(_)) => {
            AppError::new("invalid_move", "That move is illegal in this position.")
        }
        StudyError::RevisionConflict { .. } => {
            AppError::new("stale_revision", "The attempt changed. Reload to continue.")
        }
        _ => domain("invalid_submission", error),
    }
}

#[cfg(test)]
mod tests {
    use super::AppError;
    use tempo_store::StoreError;

    #[test]
    fn game_history_on_study_fixture_reports_a_nonretryable_capability_error() {
        let mut app = crate::Tempo::demo().unwrap();
        let before = app.snapshot().unwrap();
        let error = app.game_history().unwrap_err();
        assert_eq!(error.code, "unsupported_capability");
        assert!(!error.retryable);
        assert_eq!(app.snapshot().unwrap(), before);
    }

    #[test]
    fn adapter_failures_keep_recovery_distinct_from_invalid_input() {
        let conflict: AppError = StoreError::Conflict("another commit won".into()).into();
        let invalid: AppError = StoreError::Invalid("bad transaction".into()).into();
        assert_eq!(conflict.code, "store_conflict");
        assert!(conflict.retryable);
        assert_eq!(invalid.code, "store_invalid");
        assert!(!invalid.retryable);
        assert!(conflict.to_string().contains("another commit won"));
    }
}
