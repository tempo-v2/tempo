//! Normalize game records and coordinate answer-safe comparison and persistence.

use super::{Tempo, repertoires::learner_color};
use crate::adapters::public_games::FetchedGames;
use crate::game_evidence::{GameImport, GameReport, GameResult, check_id, fingerprint};
use crate::{AppError, app_error::domain};
use chrono::{NaiveDate, NaiveDateTime};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempo_chess::{LearnerColor, PlayedGame, compare_played_game, parse_played_games};
use tempo_learning::{GameMiss, GameReviewEffect, LearningState};
use tempo_store::{
    AcceptedGames, Commit, ComparedGame, GameObservation, GameReceipt, GameRecord, GameSummary,
    PgnTiming, Snapshot, Write,
};
use tempo_types::{ContentVersion, DatasetId, TargetId};

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
        let (snapshot, _) = self.store.content_state()?;
        let results: Vec<_> = history
            .results
            .into_iter()
            .map(|entry| {
                let effect = summarize_effects(entry.deviations.into_iter().map(|deviation| {
                    let miss = GameMiss {
                        answer_version: deviation.version,
                        earliest_ms: deviation.start_ms,
                        latest_ms: deviation.end_ms,
                    };
                    game_state(&snapshot, &deviation.target, deviation.version)
                        .map_or(GameReviewEffect::StaleContent, |prior| {
                            prior.game_miss_effect(miss)
                        })
                }));
                GameResult {
                    comparison_id: Some(entry.comparison_id),
                    summary: entry.summary,
                    effect,
                }
            })
            .collect();
        Ok(
            json!({"results":results,"last_successful_refresh_ms":history.last_successful_refresh_ms}),
        )
    }

    pub(crate) fn replay_game(
        &self,
        dataset: &DatasetId,
        id: &str,
        fingerprint: &str,
    ) -> Result<Option<GameReport>, AppError> {
        self.store
            .game_receipt(dataset, id, fingerprint)?
            .map(report_from_receipt)
            .transpose()
    }

    pub fn import_game_pgn(&mut self, request: GameImport) -> Result<GameReport, AppError> {
        check_id(&request.command_id)?;
        let key = fingerprint("pgn", &request)?;
        if let Some(report) = self.replay_game(&request.dataset_id, &request.command_id, &key)? {
            return Ok(report);
        }
        let records =
            parse_played_games(&request.pgn).map_err(|error| domain("invalid_game", error))?;
        if records.len() > 500 {
            return Err(invalid("Upload at most 500 games at a time."));
        }
        let fetched = records.len();
        let at = self.components.clock.now().unix_ms;
        let games = records
            .into_iter()
            .map(|record| {
                let played = record.map_err(|error| error.to_string())?;
                let mut game = uploaded_record(&played, at)?;
                if let Some(url) = source_url(&played) {
                    if let Some(verified) = self
                        .store
                        .verified_game(&request.dataset_id, &url, &game.mainline_signature)
                        .map_err(|error| error.to_string())?
                    {
                        game = verified;
                    }
                }
                Ok((game, played))
            })
            .collect();
        self.compare_games(
            &request.dataset_id,
            (&request.command_id, &key),
            &request.repertoire_id,
            &request.learner,
            games,
            GameReport {
                fetched,
                ..GameReport::default()
            },
            None,
        )
    }

    pub(crate) fn ingest_provider_games(
        &mut self,
        dataset: &DatasetId,
        command_id: &str,
        fingerprint: &str,
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
                if game.end_ms > at || game.start_ms.is_some_and(|start| start > game.end_ms) {
                    return Err("Provider game has an invalid occurrence interval.".into());
                }
                let record = GameRecord {
                    mainline_signature: mainline_signature(&played, &game.white, &game.black),
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
            dataset,
            (command_id, fingerprint),
            repertoire,
            learner,
            games,
            GameReport {
                fetched: fetched.received,
                errors: fetched.errors,
                ..GameReport::default()
            },
            Some(at),
        )
    }

    fn compare_games(
        &mut self,
        expected_dataset: &DatasetId,
        (command_id, fingerprint): (&str, &str),
        repertoire: &str,
        learner: &str,
        games: Vec<Result<(GameRecord, PlayedGame), String>>,
        mut report: GameReport,
        refresh_at: Option<u64>,
    ) -> Result<GameReport, AppError> {
        let learner = learner.trim().to_lowercase();
        if learner.is_empty() || learner.len() > 100 {
            return Err(invalid("Choose the learner with their exact player name."));
        }
        let (snapshot, revision) = self.store.content_state()?;
        let document = self.store.repertoire_document(repertoire)?;
        if expected_dataset != &snapshot.dataset
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
                if side != color && refresh_at.is_some() {
                    return Ok(None);
                }
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
                Ok(Some(ComparedGame {
                    game,
                    id,
                    summary,
                    observations,
                }))
            }) {
                Ok(Some(compared)) => comparisons.push(compared),
                Ok(None) => report.opposite_color_skipped += 1,
                Err(error) => report.errors.push(format!("Game {}: {error}", index + 1)),
            }
        }
        self.resolve_pgn_timing(&snapshot.dataset, &mut comparisons)?;
        let mut changes = BTreeMap::<TargetId, LearningState>::new();
        for compared in &comparisons {
            let mut game_effects = Vec::new();
            for observation in compared
                .observations
                .iter()
                .filter(|obs| obs.played != obs.prescribed)
            {
                let Some(prior) = game_state(&snapshot, &observation.target, observation.version)
                else {
                    continue;
                };
                let state = changes
                    .entry(observation.target.clone())
                    .or_insert_with(|| prior.clone());
                let outcome = state
                    .observe_game_miss(GameMiss {
                        answer_version: observation.version,
                        earliest_ms: compared.game.start_ms,
                        latest_ms: compared.game.end_ms,
                    })
                    .map_err(|e| domain("learning_error", e))?;
                game_effects.push(outcome);
            }
            report.matches += compared.summary.matches;
            report.deviations += compared.summary.deviations;
            report.results.push(GameResult {
                comparison_id: Some(compared.id.clone()),
                summary: compared.summary.clone(),
                effect: summarize_effects(game_effects),
            });
        }
        let reply =
            serde_json::to_string(&report).map_err(|error| domain("invalid_game_report", error))?;
        let stored = self.store.commit_games(
            Commit {
                expected_dataset: snapshot.dataset,
                expected_revision: snapshot.revision,
                writes: changes
                    .into_iter()
                    .filter(|(target, state)| snapshot.learning[target] != *state)
                    .map(|(target, state)| Write::Learning { target, state })
                    .collect(),
            },
            &comparisons,
            refresh_at.filter(|_| report.errors.is_empty()),
            GameReceipt {
                id: command_id,
                fingerprint,
                report: &reply,
            },
        )?;
        report_from_receipt(stored)
    }

    // Resolve all occurrences before projecting any report or Learning effect.
    // Store rechecks these effective times against concurrent writes at commit.
    fn resolve_pgn_timing(
        &self,
        dataset: &DatasetId,
        comparisons: &mut [ComparedGame],
    ) -> Result<(), AppError> {
        let mut timings = BTreeMap::<String, PgnTiming>::new();
        for compared in comparisons.iter() {
            let game = &compared.game;
            let Some(incoming) = game.precise_pgn_timing() else {
                continue;
            };
            let current = match timings.entry(game.id.clone()) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => entry.insert(
                    self.store
                        .pgn_timing(dataset, &game.id)?
                        .unwrap_or_else(|| incoming.clone()),
                ),
            };
            current.merge_end(&incoming)?;
        }
        for compared in comparisons {
            if compared.game.source == "pgn"
                && let Some(timing) = timings.get(&compared.game.id)
            {
                compared.game.end_ms = timing.end_ms;
                compared.summary.occurrence_end_ms = timing.end_ms;
            }
        }
        Ok(())
    }
}

fn report_from_receipt(receipt: AcceptedGames) -> Result<GameReport, AppError> {
    let mut report: GameReport = serde_json::from_str(&receipt.report)
        .map_err(|error| domain("invalid_game_report", error))?;
    report.new_games = receipt.counts.new_games;
    report.duplicates = receipt.counts.duplicates;
    Ok(report)
}

fn game_state<'a>(
    snapshot: &'a Snapshot,
    target: &TargetId,
    version: ContentVersion,
) -> Option<&'a LearningState> {
    let item = snapshot.catalog.items.get(target)?;
    if !item.active() || item.version != version {
        return None;
    }
    snapshot.learning.get(target)
}

fn summarize_effects(effects: impl IntoIterator<Item = GameReviewEffect>) -> GameReviewEffect {
    effects
        .into_iter()
        .max_by_key(|effect| match effect {
            GameReviewEffect::RequestedReview => 4,
            GameReviewEffect::PrioritizedTeaching => 3,
            GameReviewEffect::AlreadyAddressed => 2,
            GameReviewEffect::StaleContent => 1,
            GameReviewEffect::RecordedOnly => 0,
        })
        .unwrap_or(GameReviewEffect::RecordedOnly)
}

fn mainline_signature(game: &PlayedGame, white: &str, black: &str) -> String {
    let identity = json!([
        white.to_lowercase(),
        black.to_lowercase(),
        game.mainline_identity(),
        game.result
    ]);
    blake3::hash(identity.to_string().as_bytes())
        .to_hex()
        .to_string()
}

fn source_url(game: &PlayedGame) -> Option<String> {
    // Tags nominate a provider record. Only Store's matching verified row
    // can establish source identity or chronology.
    if let Some(site) = game.tags.get("Site") {
        if let Some(id) = site.strip_prefix("https://lichess.org/") {
            let id = id.strip_prefix("game/").unwrap_or(id);
            if id.len() == 8 && id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
                return Some(format!("https://lichess.org/game/{id}"));
            }
        }
        if site.starts_with("https://www.chess.com/game/") {
            return Some(site.clone());
        }
    }
    game.tags
        .get("Link")
        .filter(|url| url.starts_with("https://www.chess.com/game/"))
        .cloned()
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
    if end_ms.is_some_and(|end| start_ms.is_some_and(|start| end < start) || end > fetched_ms) {
        return Err("PGN has an invalid occurrence interval.".into());
    }
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
        mainline_signature: mainline_signature(game, &white, &black),
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
