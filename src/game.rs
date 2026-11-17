//! Already-fetched game data entering the application comparison workflow.
/// A provider's game identity, observation time, and PGN text.
///
/// A [`crate::GameProvider`] produces this record. [`crate::Tempo::ingest_games`]
/// compares its PGN against the current catalog and derives stable feedback
/// identities from provider, game ID and target. Keep those identities stable
/// when rescanning the same game so committed feedback can replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Game {
    /// Stable provider namespace, forwarded into feedback identity.
    pub provider: String,
    /// Provider-local game identity; keep it stable on every scan.
    pub id: String,
    /// Timestamp used for range filtering and observation provenance.
    pub observed_at_ms: u64,
    /// Raw content passed to the injected GameComparator.
    pub pgn: String,
}

#[cfg(test)]
mod tests {
    use super::Game;
    use crate::Tempo;

    #[test]
    fn stable_provider_and_game_identity_make_rescanning_the_same_game_replayable() {
        let games = [Game {
            provider: "club".into(),
            id: "game-42".into(),
            observed_at_ms: 5,
            pgn: tempo_chess::GAME_PGN.into(),
        }];
        let mut app = Tempo::demo().unwrap();
        app.ingest_games(&games).unwrap();
        let revision = app.snapshot().unwrap().revision;
        app.ingest_games(&games).unwrap();
        assert_eq!(app.snapshot().unwrap().revision, revision);
    }
}
