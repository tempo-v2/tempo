//! Convert already-fetched games into versioned external miss commands.
use super::Tempo;
use crate::{AgainCommand, AppError, Game, app_error::domain};
use tempo_types::EventId;

impl Tempo {
    /// Compare each fetched game against the current catalog and send each
    /// deviation through [`Self::again`]. The count is processed deviations,
    /// including replayed ones; it is not the number of newly inserted events.
    /// Each deviation commits independently; a later failure does not roll back
    /// earlier events from this batch.
    /// Accept already-fetched games. Provider I/O happens outside this application object.
    pub fn ingest_games(&mut self, games: &[Game]) -> Result<usize, AppError> {
        if self.fixture.is_none() {
            return Err(AppError::new(
                "unsupported_capability",
                "Use Games to compare accepted games in Daily Study.",
            ));
        }
        let mut count = 0;
        for game in games {
            let snapshot = self.store.snapshot()?;
            let deviations = self
                .fixture
                .as_ref()
                .expect("fixture app")
                .comparator
                .compare(&game.pgn, &snapshot.catalog)
                .map_err(|e| domain("game_comparison_error", e))?;
            for deviation in deviations {
                let event = serde_json::to_string(&(&game.id, &deviation.target))
                    .map_err(|e| domain("codec_error", e))?;
                let version = snapshot
                    .catalog
                    .items
                    .get(&deviation.target)
                    .ok_or_else(|| {
                        AppError::new("unknown_target", "comparison referenced unknown content")
                    })?
                    .version;
                self.again(AgainCommand {
                    dataset_id: snapshot.dataset.clone(),
                    target: deviation.target,
                    expected_content_version: version,
                    provider: game.provider.clone(),
                    event_id: EventId(event),
                    observed_at_ms: game.observed_at_ms,
                })?;
                count += 1;
            }
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use crate::{Game, Tempo};

    #[test]
    fn scan_count_includes_replayed_deviations_but_storage_does_not_duplicate_them() {
        let mut app = Tempo::demo().unwrap();
        let games = [Game {
            provider: "example".into(),
            id: "42".into(),
            observed_at_ms: 5,
            pgn: tempo_chess::GAME_PGN.into(),
        }];
        assert_eq!(app.ingest_games(&games).unwrap(), 1);
        assert_eq!(app.ingest_games(&games).unwrap(), 1);
        assert_eq!(app.snapshot().unwrap().assessments.len(), 1);
    }
}
