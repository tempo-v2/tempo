use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorView {
    pub schema_version: u32,
    pub status: String,
    pub code: String,
    pub message: String,
    pub retryable: bool,
}
