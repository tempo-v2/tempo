//! Select the observation-time window passed to a game provider.
/// An inclusive range of observation timestamps in Unix milliseconds.
///
/// A refresh job uses this window to filter fetched games.
/// The provider validates ordering; constructing the value alone does not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameRange {
    pub from_ms: u64,
    pub to_ms: u64,
}
