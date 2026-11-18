//! Replaceable application seams; implementations are selected by `Components`.
pub mod attempt_ids;
pub mod chess_grader;
pub mod clock;
pub mod game_provider;
pub mod snapshot_codec;

pub use attempt_ids::AttemptIds;
pub use chess_grader::ChessGrader;
pub use clock::Clock;
pub use game_provider::GameProvider;
pub use snapshot_codec::SnapshotCodec;
