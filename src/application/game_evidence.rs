//! Normalize game records and coordinate answer-safe comparison and persistence.

use super::{Tempo, repertoires::learner_color};
use crate::adapters::public_games::FetchedGames;
use crate::game_evidence::{GameEffect, GameImport, GameReport, GameResult};
use crate::{AppError, app_error::domain};
use chrono::{NaiveDate, NaiveDateTime};
use serde_json::{Value, json};
use tempo_chess::{LearnerColor, PlayedGame, compare_played_game, parse_played_games};
use tempo_store::{Commit, ComparedGame, GameObservation, GameRecord, GameSummary};
use tempo_types::DatasetId;

impl Tempo {
    pub(crate) fn game_repertoire_dataset(&self, id: &str) -> Result<DatasetId, AppError> {
        let document = self.store.repertoire_document(id)?;
        if document.item.is_none() {
            return Err(invalid("Choose an existing repertoire."));
        }
        Ok(document.dataset)
    }

    pub fn game_history(&self) -> Result<Value, AppError> {
        let history = self.store.game_history()?;
        let results: Vec<_> = history.results.into_iter().map(view).collect();
        Ok(
            json!({"results":results,"last_successful_refresh_ms":history.last_successful_refresh_ms}),
        )
    }

    pub fn import_game_pgn(&mut self, request: GameImport) -> Result<GameReport, AppError> {
        let records =
            parse_played_games(&request.pgn).map_err(|error| domain("invalid_game", error))?;
        if records.len() > 500 {
            return Err(invalid("Upload at most 500 games at a time."));
        }
        let at = self.components.clock.now().unix_ms;
        let games = records
            .into_iter()
            .map(|record| {
                let played = record.map_err(|error| error.to_string())?;
                let game = uploaded_record(&played, at)?;
                Ok((game, played))
            })
            .collect();
        self.compare_games(
            None,
            &request.repertoire_id,
            &request.learner,
            games,
            Vec::new(),
            None,
        )
    }

    pub(crate) fn ingest_provider_games(
        &mut self,
        dataset: &DatasetId,
        repertoire: &str,
        learner: &str,
        fetched: FetchedGames,
    ) -> Result<GameReport, AppError> {
        let at = self.components.clock.now().unix_ms;
        let games = fetched
            .games
            .into_iter()
            .map(|game| {
                let mut records =
                    parse_played_games(&game.pgn).map_err(|error| error.to_string())?;
                if records.len() != 1 {
                    return Err("Provider record must contain exactly one finished game.".into());
                }
                let played = records.pop().unwrap().map_err(|error| error.to_string())?;
                let record = GameRecord {
                    source: game.provider.into(),
                    id: game.id,
                    url: Some(game.url),
                    white: game.white,
                    black: game.black,
                    start_ms: game.start_ms,
                    end_ms: Some(game.end_ms),
                    fetched_ms: at,
                    pgn: game.pgn,
                };
                Ok((record, played))
            })
            .collect();
        self.compare_games(
            Some(dataset),
            repertoire,
            learner,
            games,
            fetched.errors,
            Some(at),
        )
    }

    fn compare_games(
        &mut self,
        expected_dataset: Option<&DatasetId>,
        repertoire: &str,
        learner: &str,
        games: Vec<Result<(GameRecord, PlayedGame), String>>,
        errors: Vec<String>,
        refresh_at: Option<u64>,
    ) -> Result<GameReport, AppError> {
        let learner = learner.trim().to_lowercase();
        if learner.is_empty() || learner.len() > 100 {
            return Err(invalid("Choose the learner with their exact player name."));
        }
        let (snapshot, revision) = self.store.content_state()?;
        let document = self.store.repertoire_document(repertoire)?;
        if expected_dataset.is_some_and(|dataset| dataset != &snapshot.dataset)
            || document.dataset != snapshot.dataset
            || document.revision != revision
        {
            return Err(invalid("The dataset or repertoire changed during refresh."));
        }
        let item = document
            .item
            .as_ref()
            .ok_or_else(|| invalid("Choose an existing repertoire."))?;
        let color = learner_color(item);
        let mut report = GameReport {
            fetched: games.len() + errors.len(),
            errors,
            ..GameReport::default()
        };
        let mut comparisons = Vec::new();
        for (index, record) in games.into_iter().enumerate() {
            match record.and_then(|(game, played)| {
                let side = match (
                    game.white.to_lowercase() == learner,
                    game.black.to_lowercase() == learner,
                ) {
                    (true, false) => LearnerColor::White,
                    (false, true) => LearnerColor::Black,
                    _ => return Err("The learner does not identify exactly one player.".into()),
                };
                if side != color {
                    return Err(
                        "The learner played the opposite color from the selected repertoire."
                            .into(),
                    );
                }
                let comparison = compare_played_game(
                    &played,
                    &snapshot.catalog,
                    repertoire,
                    color,
                    document.pgn.as_deref(),
                )
                .map_err(|error| error.to_string())?;
                let observations: Vec<_> = comparison
                    .observations
                    .into_iter()
                    .map(|obs| GameObservation {
                        ply: obs.ply,
                        target: obs.target,
                        version: obs.version,
                        played: obs.played,
                        prescribed: obs.prescribed,
                    })
                    .collect();
                let versions: Vec<_> = observations
                    .iter()
                    .map(|obs| (obs.ply, &obs.target, obs.version))
                    .collect();
                let first_deviation_ply = observations
                    .iter()
                    .find(|obs| obs.played != obs.prescribed)
                    .map(|obs| obs.ply);
                let deviations = observations
                    .iter()
                    .filter(|obs| obs.played != obs.prescribed)
                    .count();
                let summary = GameSummary {
                    source: game.source.clone(),
                    game_id: game.id.clone(),
                    url: game.url.clone(),
                    repertoire_id: repertoire.into(),
                    learner: learner.clone(),
                    occurrence_end_ms: game.end_ms,
                    matches: observations.len() - deviations,
                    deviations,
                    first_deviation_ply,
                    coverage_gaps: comparison.coverage_gaps,
                    opponent_deviations: comparison.opponent_deviations,
                };
                let key = json!([
                    game.source,
                    game.id,
                    learner,
                    repertoire,
                    versions,
                    summary.coverage_gaps,
                    summary.opponent_deviations
                ]);
                let id = blake3::hash(key.to_string().as_bytes())
                    .to_hex()
                    .to_string();
                Ok(ComparedGame {
                    game,
                    id,
                    summary,
                    observations,
                })
            }) {
                Ok(compared) => comparisons.push(compared),
                Err(error) => report.errors.push(format!("Game {}: {error}", index + 1)),
            }
        }
        let stored = self.store.commit_games(
            Commit {
                expected_dataset: snapshot.dataset,
                expected_revision: snapshot.revision,
                writes: Vec::new(),
            },
            &comparisons,
            refresh_at.filter(|_| report.errors.is_empty()),
        )?;
        report.new_games = stored.new_games;
        report.duplicates = stored.duplicates;
        for comparison in comparisons {
            report.matches += comparison.summary.matches;
            report.deviations += comparison.summary.deviations;
            report.results.push(view(comparison.summary));
        }
        Ok(report)
    }
}

fn view(summary: GameSummary) -> GameResult {
    GameResult {
        summary,
        effect: GameEffect::RecordedOnly,
    }
}

fn uploaded_record(game: &PlayedGame, fetched_ms: u64) -> Result<GameRecord, String> {
    let tag = |name| game.tags.get(name).map(String::as_str);
    let name = |side| {
        tag(side)
            .filter(|name| !matches!(name.trim(), "" | "?" | "-"))
            .ok_or_else(|| format!("PGN requires an identified {side} player."))
    };
    let white = name("White")?.trim().to_owned();
    let black = name("Black")?.trim().to_owned();
    let date = tag("UTCDate").or_else(|| tag("Date"));
    let start_ms = utc_ms(tag("UTCDate"), tag("UTCTime"));
    let end_ms = utc_ms(tag("UTCEndDate"), tag("UTCEndTime")).and_then(|ms| ms.checked_add(999));
    let end_ms =
        end_ms.filter(|end| start_ms.is_none_or(|start| start <= *end) && *end <= fetched_ms);
    let (start_position, moves) = game.mainline_identity();
    let identity = json!([
        white.to_lowercase(),
        black.to_lowercase(),
        date,
        start_ms,
        start_position,
        moves,
        game.result
    ]);
    let id = blake3::hash(identity.to_string().as_bytes())
        .to_hex()
        .to_string();
    // Preserve the legal played facts for a local upload without retaining unrelated analysis.
    let setup = tag("FEN").map_or_else(String::new, |fen| {
        format!("[SetUp \"1\"]\n[FEN {}]\n", json!(fen))
    });
    let pgn = format!(
        "[White {}]\n[Black {}]\n[Result {}]\n{setup}\n{}\n",
        json!(white),
        json!(black),
        json!(game.result),
        game.mainline_pgn()
    );
    Ok(GameRecord {
        source: "pgn".into(),
        id,
        url: None,
        white,
        black,
        start_ms,
        end_ms,
        fetched_ms,
        pgn,
    })
}

fn utc_ms(date: Option<&str>, time: Option<&str>) -> Option<u64> {
    let date = NaiveDate::parse_from_str(date?, "%Y.%m.%d").ok()?;
    let timestamp =
        NaiveDateTime::parse_from_str(&format!("{date} {}", time?), "%Y-%m-%d %H:%M:%S").ok()?;
    timestamp.and_utc().timestamp_millis().try_into().ok()
}

fn invalid(message: impl Into<String>) -> AppError {
    AppError::new("invalid_game", message)
}
