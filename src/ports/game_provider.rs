//! Fetch games outside the object that coordinates study.
use crate::{AppError, Game, GameRange};

/// Fetch games outside the object that coordinates study.
///
/// Use this port in a polling job or an explicit refresh action. The caller first
/// fetches a time range, then passes the returned games to [`crate::Tempo::ingest_games`].
/// A provider failure is separate from asking Tempo for the next exercise.
pub trait GameProvider {
    fn fetch(&self, range: &GameRange) -> Result<Vec<Game>, AppError>;
}

#[cfg(test)]
mod tests {
    use super::GameProvider;
    use crate::{AppError, Game, GameRange, Tempo};

    struct OneFetchedGame;
    impl GameProvider for OneFetchedGame {
        fn fetch(&self, _: &GameRange) -> Result<Vec<Game>, AppError> {
            Ok(vec![Game {
                provider: "example-provider".into(),
                id: "42".into(),
                observed_at_ms: 5,
                pgn: tempo_chess::GAME_PGN.into(),
            }])
        }
    }

    #[test]
    fn a_refresh_caller_fetches_first_then_passes_plain_games_to_tempo() {
        let provider: &dyn GameProvider = &OneFetchedGame;
        let games = provider
            .fetch(&GameRange {
                from_ms: 0,
                to_ms: 10,
            })
            .unwrap();
        let mut app = Tempo::demo().unwrap();
        assert_eq!(app.ingest_games(&games).unwrap(), 1);
        assert_eq!(app.snapshot().unwrap().assessments.len(), 1);
        assert!(app.snapshot().unwrap().attempts.is_empty());
    }
}
