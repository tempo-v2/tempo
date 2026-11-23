//! Small fixture implementations of the application ports.
pub mod counter_ids;
pub mod fixed_clock;
pub mod json_snapshot_codec;
pub use counter_ids::CounterIds;
pub use fixed_clock::FixedClock;
pub use json_snapshot_codec::JsonSnapshotCodec;

pub(crate) mod public_games;
