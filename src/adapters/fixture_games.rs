//! An in-memory game provider filtered by an inclusive time range.
use crate::{AppError, Game, GameProvider, GameRange};

/// An in-memory game provider filtered by an inclusive time range.
///
/// Use this to exercise refresh and ingestion without network I/O. Supplied PGN
/// text still goes through the injected game comparator after fetching.
pub struct FixtureGames(pub Vec<Game>);
impl GameProvider for FixtureGames {
    fn fetch(&self, range: &GameRange) -> Result<Vec<Game>, AppError> {
        if range.from_ms > range.to_ms {
            return Err(AppError::new("invalid_range", "start follows end"));
        }
        Ok(self
            .0
            .iter()
            .filter(|g| g.observed_at_ms >= range.from_ms && g.observed_at_ms <= range.to_ms)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::FixtureGames;
    use crate::{Game, GameProvider, GameRange};

    #[test]
    fn a_scan_selects_both_range_endpoints_and_excludes_other_observations() {
        let provider = FixtureGames(
            [9, 10, 20, 21]
                .into_iter()
                .map(|observed_at_ms| Game {
                    provider: "fixture".into(),
                    id: observed_at_ms.to_string(),
                    observed_at_ms,
                    pgn: tempo_chess::GAME_PGN.into(),
                })
                .collect(),
        );
        let fetched = provider
            .fetch(&GameRange {
                from_ms: 10,
                to_ms: 20,
            })
            .unwrap();
        assert_eq!(
            fetched
                .iter()
                .map(|game| game.observed_at_ms)
                .collect::<Vec<_>>(),
            vec![10, 20]
        );
    }
}
