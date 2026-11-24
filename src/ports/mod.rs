//! Replaceable application seams; implementations are selected by `DailyComponents`.
pub mod attempt_ids;
pub mod chess_grader;
pub mod clock;

pub use attempt_ids::AttemptIds;
pub use chess_grader::ChessGrader;
pub use clock::Clock;
