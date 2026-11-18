//! A fixture provider that always returns `provider_unavailable`.
use crate::{AppError, Game, GameProvider, GameRange};

/// A fixture provider that always returns `provider_unavailable`.
///
/// Use this to demonstrate that optional game refresh can fail while ordinary
/// study continues. The caller decides when to schedule another refresh.
pub struct UnavailableGames;
impl GameProvider for UnavailableGames {
    fn fetch(&self, _: &GameRange) -> Result<Vec<Game>, AppError> {
        Err(AppError::new(
            "provider_unavailable",
            "fixture provider is unavailable",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::UnavailableGames;
    use crate::{GameProvider, GameRange, Tempo};

    #[test]
    fn an_optional_refresh_failure_does_not_prevent_starting_study() {
        let error = UnavailableGames
            .fetch(&GameRange {
                from_ms: 0,
                to_ms: 10,
            })
            .unwrap_err();
        assert_eq!(error.code, "provider_unavailable");
        let mut app = Tempo::demo().unwrap();
        assert!(app.next_exercise().unwrap().is_some());
    }
}
