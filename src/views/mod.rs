//! Values sent to the UI. Follow each file to its fields and executable usage.
pub mod attempt_view;
pub mod empty_view;
pub mod error_view;
pub mod study_view;

pub use attempt_view::AttemptView;
pub use empty_view::EmptyView;
pub use error_view::ErrorView;
pub use study_view::StudyView;
