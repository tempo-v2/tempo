//! Small fixture implementations of the application ports.
pub mod counter_ids;
pub mod fixed_clock;
pub use counter_ids::CounterIds;
pub use fixed_clock::FixedClock;

pub(crate) mod public_games;
