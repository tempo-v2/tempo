use serde::{Deserialize, Serialize};

use crate::views::{attempt_view::AttemptView, empty_view::EmptyView, error_view::ErrorView};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StudyView {
    Attempt(AttemptView),
    Empty(EmptyView),
    Error(ErrorView),
}
