//! Select the observation-time window passed to a game provider.
/// An inclusive range of observation timestamps in Unix milliseconds.
///
/// Construct this in a refresh job and pass it to [`crate::GameProvider::fetch`].
/// The provider validates ordering; constructing the value alone does not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameRange {
    pub from_ms: u64,
    pub to_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::GameRange;
    use crate::{FixtureGames, GameProvider};

    #[test]
    fn an_inverted_scan_window_is_rejected_by_the_provider() {
        let range = GameRange {
            from_ms: 20,
            to_ms: 10,
        };
        let error = FixtureGames(vec![]).fetch(&range).unwrap_err();
        assert_eq!(error.code, "invalid_range");
    }
}
