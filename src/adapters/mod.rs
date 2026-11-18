//! Small fixture implementations of the application ports.
pub mod counter_ids;
pub mod fixed_clock;
pub mod fixture_games;
pub mod json_snapshot_codec;
pub mod unavailable_games;

pub use counter_ids::CounterIds;
pub use fixed_clock::FixedClock;
pub use fixture_games::FixtureGames;
pub use json_snapshot_codec::JsonSnapshotCodec;
pub use unavailable_games::UnavailableGames;

pub(crate) mod public_games;
