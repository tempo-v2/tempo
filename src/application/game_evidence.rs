//! Normalize game records and coordinate answer-safe comparison and persistence.

use super::{Tempo, repertoires::learner_color};
use crate::adapters::public_games::FetchedGames;
use crate::game_evidence::{GameImport, GameReport, GameResult, check_id, fingerprint};
use crate::{AppError, app_error::domain};
use chrono::{NaiveDate, NaiveDateTime};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tempo_chess::{
    ContentKind, DecisionAdherence, LearnerColor, PlayedGame, compare_active_repertoires,
    parse_played_games,
};
use tempo_learning::{
    AssessmentInput, AssessmentSource, DailyLearning, GameReviewEffect, LearningState,
};
use tempo_store::{
    AcceptedGames, AssessmentKey, AssessmentRecord, Commit, ComparedGame, GameChoice,
    GameChoiceTarget, GameFailureKey, GameObservation, GameReceipt, GameRecord, GameSummary,
    PgnTiming, RepertoireDisagreement, Write,
};
use tempo_types::{DatasetId, EventId, Grade, TargetId};

impl Tempo {
    pub(crate) fn game_dataset(&self) -> Result<DatasetId, AppError> {
        Ok(self.store.study_state(Default::default())?.dataset)
    }

    pub fn game_history(&self) -> Result<Value, AppError> {
        let history = self.store.game_history()?;
        let results: Vec<_> = history
            .results
            .into_iter()
            .map(|entry| GameResult {
                comparison_id: Some(entry.comparison_id),
                effect: entry
                    .summary
                    .correction_effect
                    .unwrap_or(GameReviewEffect::RecordedOnly),
                summary: entry.summary,
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
            &request.learner,
            games,
            GameReport {
                fetched,
                ..GameReport::default()
            },
            None,
            None,
            None,
        )
    }

    pub(crate) fn ingest_provider_games(
        &mut self,
        dataset: &DatasetId,
        command_id: &str,
        fingerprint: &str,
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
                    played_date_ms: None,
                    pgn: game.pgn,
                };
                Ok((record, played))
            })
            .collect();
        self.compare_games(
            dataset,
            (command_id, fingerprint),
            learner,
            games,
            GameReport {
                fetched: fetched.received,
                errors: fetched.errors,
                ..GameReport::default()
            },
            Some(at),
            None,
            None,
        )
    }

    pub fn choose_game_correction(
        &mut self,
        request: crate::game_evidence::ChooseGameCorrection,
    ) -> Result<GameReport, AppError> {
        check_id(&request.command_id)?;
        let fingerprint = fingerprint("choose_correction", &request)?;
        if let Some(report) =
            self.replay_game(&request.dataset_id, &request.command_id, &fingerprint)?
        {
            return Ok(report);
        }
        let saved = self
            .store
            .game_comparison(&request.dataset_id, &request.comparison_id)?
            .filter(|result| {
                result.source == request.source
                    && result.game_id == request.game_id
                    && result.learner == request.learner
            })
            .ok_or_else(|| invalid("The comparison was not found."))?;
        let choice = saved
            .choices
            .iter()
            .find(|choice| {
                choice.ply == request.ply
                    && choice
                        .candidates
                        .iter()
                        .any(|candidate| candidate.target_id == request.target_id)
            })
            .ok_or_else(|| invalid("Choose a target from the saved miss."))?;
        if self.store.game_choice_resolved(
            &request.dataset_id,
            &request.source,
            &request.game_id,
            &choice.position,
        )? {
            return Err(invalid("This corrective position has already been chosen."));
        }
        let game = self
            .store
            .stored_game(&request.dataset_id, &request.source, &request.game_id)?
            .ok_or_else(|| invalid("This game was not found."))?;
        self.compare_games(
            &request.dataset_id,
            (&request.command_id, &fingerprint),
            &request.learner,
            vec![parse_stored_game(game)],
            GameReport {
                fetched: 1,
                ..Default::default()
            },
            None,
            Some(&SelectedCorrection {
                comparison_id: request.comparison_id,
                ply: request.ply,
                target: request.target_id,
                position: choice.position.clone(),
            }),
            None,
        )
    }

    pub fn reprocess_games(
        &mut self,
        request: crate::game_evidence::ReprocessGames,
    ) -> Result<GameReport, AppError> {
        check_id(&request.command_id)?;
        let fingerprint = fingerprint("reprocess", &request)?;
        if let Some(report) =
            self.replay_game(&request.dataset_id, &request.command_id, &fingerprint)?
        {
            return Ok(report);
        }
        if request.to_ms <= request.from_ms || request.to_ms - request.from_ms > 31 * 86_400_000 {
            return Err(invalid("Choose a window of at most 31 days."));
        }
        let games =
            self.store
                .games_in_window(&request.dataset_id, request.from_ms, request.to_ms)?;
        if games.len() > 500 {
            return Err(invalid(
                "This window contains more than 500 games. Choose a shorter window.",
            ));
        }
        let learner = request.learner.trim().to_lowercase();
        let games: Vec<_> = games
            .into_iter()
            .filter(|game| {
                [&game.white, &game.black]
                    .into_iter()
                    .any(|player| player.to_lowercase() == learner)
            })
            .map(parse_stored_game)
            .collect();
        let fetched = games.len();
        self.compare_games(
            &request.dataset_id,
            (&request.command_id, &fingerprint),
            &request.learner,
            games,
            GameReport {
                fetched,
                ..Default::default()
            },
            None,
            None,
            None,
        )
    }

    /// Upgrade only outstanding legacy obligations backed by stored evidence.
    /// Each game transaction can safely be replayed if startup was interrupted.
    pub fn reconcile_legacy_game_requests(&mut self) -> Result<(), AppError> {
        let (snapshot, _) = self.store.content_state()?;
        let mut batches = BTreeMap::<_, BTreeSet<_>>::new();
        let mut reconciled = BTreeSet::new();
        for (target, state) in &snapshot.learning {
            if state.game_review.is_none() && !state.game_teaching_relevance {
                continue;
            }
            let item = &snapshot.catalog.items[target];
            let misses = self
                .store
                .legacy_game_misses(&snapshot.dataset, target, item.version)?;
            if misses.is_empty() {
                continue;
            }
            reconciled.insert(target.clone());
            for miss in misses {
                let previously_pending = state.game_teaching_relevance
                    || state.game_review.is_some_and(|review| {
                        review.answer_version == item.version
                            && miss
                                .start_ms
                                .zip(state.taught_at)
                                .is_some_and(|(start, taught)| start > taught.unix_ms)
                            && miss.end_ms.is_some_and(|end| {
                                end <= review.occurred_by_ms
                                    && state
                                        .last_assessed_test_issue_ms
                                        .is_none_or(|issue| end >= issue)
                            })
                    });
                if previously_pending && item.active() {
                    batches
                        .entry((miss.source, miss.game_id, miss.learner))
                        .or_default()
                        .insert(miss.ply);
                }
            }
        }
        for ((source, id, learner), plies) in batches {
            let key = hash(&json!([
                "legacy-game-reconciliation",
                source,
                id,
                learner,
                plies
            ]));
            let command_id = format!("legacy-{key}");
            if self
                .replay_game(&snapshot.dataset, &command_id, &key)?
                .is_some()
            {
                continue;
            }
            let game = self
                .store
                .stored_game(&snapshot.dataset, &source, &id)?
                .ok_or_else(|| invalid("A pending request refers to a missing game."))?;
            self.compare_games(
                &snapshot.dataset,
                (&command_id, &key),
                &learner,
                vec![parse_stored_game(game)],
                GameReport {
                    fetched: 1,
                    ..Default::default()
                },
                None,
                None,
                Some(&plies),
            )?;
        }
        if !reconciled.is_empty() {
            let (current, _) = self.store.content_state()?;
            let writes = reconciled
                .into_iter()
                .filter_map(|target| {
                    let mut state = current.learning[&target].clone();
                    if state.game_review.take().is_some() || state.game_teaching_relevance {
                        state.game_teaching_relevance = false;
                        Some(Write::Learning { target, state })
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            if !writes.is_empty() {
                self.commit(&current, writes)?;
            }
        }
        Ok(())
    }

    fn compare_games(
        &mut self,
        expected_dataset: &DatasetId,
        (command_id, fingerprint): (&str, &str),
        learner: &str,
        mut games: Vec<Result<(GameRecord, PlayedGame), String>>,
        mut report: GameReport,
        refresh_at: Option<u64>,
        selection: Option<&SelectedCorrection>,
        legacy_plies: Option<&BTreeSet<usize>>,
    ) -> Result<GameReport, AppError> {
        let learner = learner.trim().to_lowercase();
        if learner.is_empty() || learner.len() > 100 {
            return Err(invalid("Choose the learner with their exact player name."));
        }
        let (snapshot, revision) = self.store.content_state()?;
        if expected_dataset != &snapshot.dataset {
            return Err(invalid("The dataset changed during refresh."));
        }
        let repertoires: BTreeSet<_> = snapshot
            .catalog
            .items
            .values()
            .filter(|item| item.active())
            .filter_map(|item| match &item.definition.kind {
                ContentKind::Repertoire { repertoire, .. } => Some(repertoire.clone()),
                _ => None,
            })
            .collect();
        let mut books = BTreeMap::new();
        let mut resets = BTreeMap::new();
        for repertoire in repertoires {
            let document = self.store.repertoire_document(&repertoire)?;
            if document.dataset != snapshot.dataset || document.revision != revision {
                return Err(invalid("A repertoire changed during refresh."));
            }
            if let Some(item) = document.item {
                books.insert(repertoire.clone(), (learner_color(&item), document.pgn));
            }
            let cutoff = self
                .store
                .repertoire_reset_at(&snapshot.dataset, &repertoire)?;
            resets.insert(repertoire, cutoff);
        }
        let documents = |color| {
            books
                .iter()
                .filter_map(|(id, (side, pgn))| {
                    (*side == color)
                        .then(|| pgn.clone().map(|pgn| (id.clone(), pgn)))
                        .flatten()
                })
                .collect()
        };
        let white_documents = documents(LearnerColor::White);
        let black_documents = documents(LearnerColor::Black);
        let mut comparisons = Vec::new();
        let mut changes = BTreeMap::<TargetId, LearningState>::new();
        let mut accepted = BTreeSet::new();
        let at = self.components.clock.now();
        self.resolve_pgn_timing(&snapshot.dataset, &mut games)?;
        for (index, record) in games.into_iter().enumerate() {
            let (game, played) = match record {
                Ok(record) => record,
                Err(error) => {
                    report.errors.push(format!("Game {}: {error}", index + 1));
                    continue;
                }
            };
            let side = match (
                game.white.to_lowercase() == learner,
                game.black.to_lowercase() == learner,
            ) {
                (true, false) => LearnerColor::White,
                (false, true) => LearnerColor::Black,
                _ => {
                    report.errors.push(format!(
                        "Game {}: The learner does not identify exactly one player.",
                        index + 1
                    ));
                    continue;
                }
            };
            let documents = match side {
                LearnerColor::White => &white_documents,
                LearnerColor::Black => &black_documents,
            };
            let decisions = compare_active_repertoires(&played, &snapshot.catalog, side, documents)
                .map_err(|error| domain("invalid_comparison", error))?;
            let versions: Vec<_> = decisions
                .iter()
                .flat_map(|decision| {
                    decision.observations.iter().map(|obs| {
                        (
                            obs.ply,
                            &obs.target,
                            obs.version,
                            &obs.played,
                            &obs.prescribed,
                        )
                    })
                })
                .collect();
            let routes: Vec<_> = decisions
                .iter()
                .filter_map(|decision| match &decision.adherence {
                    DecisionAdherence::Miss { candidates } => Some((
                        decision.ply,
                        candidates.iter().map(|o| &o.target).collect::<Vec<_>>(),
                    )),
                    _ => None,
                })
                .collect();
            let comparison_id = hash(&json!([
                "combined",
                game.source,
                game.id,
                learner,
                versions,
                routes
            ]));
            if let Some(selection) = selection
                && selection.comparison_id != comparison_id
            {
                return Err(invalid("The comparison changed. Compare the game again."));
            }
            let mut summary = GameSummary {
                source: game.source.clone(),
                game_id: game.id.clone(),
                url: game.url.clone(),
                repertoire_id: String::new(),
                learner: learner.clone(),
                occurrence_end_ms: game.end_ms,
                matches: 0,
                deviations: 0,
                first_deviation_ply: None,
                coverage_gaps: 0,
                opponent_deviations: tempo_chess::opponent_deviations(&played, side, documents)
                    .map_err(|error| domain("invalid_comparison", error))?,
                correction_effect: Some(GameReviewEffect::RecordedOnly),
                choices: vec![],
                disagreements: vec![],
            };
            let repertoire_of =
                |target: &TargetId| match &snapshot.catalog.items[target].definition.kind {
                    ContentKind::Repertoire { repertoire, .. } => repertoire.clone(),
                    _ => unreachable!("Chess returned a repertoire target"),
                };
            let mut disagreements = BTreeMap::<String, Vec<usize>>::new();
            let mut observations = Vec::new();
            let mut failures = Vec::new();
            let mut choice_groups = BTreeSet::new();
            let mut resolved = false;
            for decision in decisions {
                for observation in decision
                    .observations
                    .iter()
                    .filter(|obs| obs.played != obs.prescribed)
                {
                    disagreements
                        .entry(repertoire_of(&observation.target))
                        .or_default()
                        .push(decision.ply);
                }
                let candidates = match decision.adherence {
                    DecisionAdherence::Matched => {
                        summary.matches += 1;
                        continue;
                    }
                    DecisionAdherence::Uncovered => {
                        summary.coverage_gaps += 1;
                        continue;
                    }
                    DecisionAdherence::Miss { candidates } => candidates,
                };
                summary.deviations += 1;
                summary.first_deviation_ply.get_or_insert(decision.ply);
                if legacy_plies.is_some_and(|plies| !plies.contains(&decision.ply)) {
                    continue;
                }
                if selection.is_none()
                    && self.store.game_choice_resolved(
                        &snapshot.dataset,
                        &game.source,
                        &game.id,
                        &decision.position.0,
                    )?
                {
                    if summary.correction_effect != Some(GameReviewEffect::AgainApplied) {
                        summary.correction_effect = Some(GameReviewEffect::AlreadyAddressed);
                    }
                    continue;
                }
                let candidate = if let Some(selection) = selection {
                    if decision.ply != selection.ply || decision.position.0 != selection.position {
                        continue;
                    }
                    if candidates.len() <= 1 {
                        return Err(invalid("This miss has no unresolved choice."));
                    }
                    let chosen = candidates
                        .iter()
                        .find(|obs| obs.target == selection.target)
                        .ok_or_else(|| invalid("Choose a current target from the comparison."))?;
                    resolved = true;
                    chosen
                } else if let [only] = candidates.as_slice() {
                    only
                } else {
                    let eligible: Vec<_> = candidates
                        .iter()
                        .filter_map(|obs| {
                            let repertoire_id = repertoire_of(&obs.target);
                            self.after_game_reset(resets[&repertoire_id], &game, &played)
                                .then(|| GameChoiceTarget {
                                    repertoire_id,
                                    target_id: obs.target.clone(),
                                    version: obs.version,
                                })
                        })
                        .collect();
                    if !eligible.is_empty() && choice_groups.insert(decision.position.0.clone()) {
                        summary.choices.push(GameChoice {
                            ply: decision.ply,
                            position: decision.position.0.clone(),
                            candidates: eligible,
                        });
                    }
                    continue;
                };
                let repertoire = repertoire_of(&candidate.target);
                if !self.after_game_reset(resets[&repertoire], &game, &played) {
                    if selection.is_some() {
                        return Err(invalid("This game predates the repertoire progress reset."));
                    }
                    continue;
                }
                observations.push(GameObservation {
                    ply: candidate.ply,
                    target: candidate.target.clone(),
                    version: candidate.version,
                    played: candidate.played.clone(),
                    prescribed: candidate.prescribed.clone(),
                });
                let key = GameFailureKey {
                    source: game.source.clone(),
                    game_id: game.id.clone(),
                    target: candidate.target.clone(),
                    version: candidate.version,
                    chosen_position: selection.map(|selection| selection.position.clone()),
                };
                if self.store.game_failed(&snapshot.dataset, &key)?
                    || !accepted.insert((
                        key.source.clone(),
                        key.game_id.clone(),
                        key.target.clone(),
                    ))
                {
                    if summary.correction_effect != Some(GameReviewEffect::AgainApplied) {
                        summary.correction_effect = Some(GameReviewEffect::AlreadyAddressed);
                    }
                    continue;
                }
                let prior = changes
                    .entry(key.target.clone())
                    .or_insert_with(|| snapshot.learning[&key.target].clone());
                let provider = format!("game:{}", key.source);
                let event_id = EventId(hash(&json!([key.game_id, key.target])));
                let input = AssessmentInput {
                    grade: Grade::Again,
                    accepted_at: at,
                    source: AssessmentSource::Game {
                        source: key.source.clone(),
                        game_id: key.game_id.clone(),
                        played_date: parsed_game_date(&played).map(|date| date.to_string()),
                        started_at_ms: game.start_ms,
                        ended_at_ms: game.end_ms,
                    },
                };
                let mut transition = DailyLearning::assess_game(
                    self.components.learning.as_ref(),
                    prior,
                    &input,
                    key.version,
                    decision.previous_move,
                )
                .map_err(|error| domain("learning_error", error))?;
                if legacy_plies.is_none() {
                    transition.state.game_review = None;
                    transition.state.game_teaching_relevance = false;
                }
                *prior = transition.state.clone();
                failures.push(AssessmentRecord {
                    key: AssessmentKey::External { provider, event_id },
                    dataset: snapshot.dataset.clone(),
                    target: key.target,
                    content_version: key.version,
                    input,
                    transition,
                });
                summary.correction_effect = Some(GameReviewEffect::AgainApplied);
            }
            if selection.is_some() && !resolved {
                return Err(invalid("This miss is no longer available."));
            }
            summary.disagreements = disagreements
                .into_iter()
                .map(|(repertoire_id, plies)| RepertoireDisagreement {
                    repertoire_id,
                    plies,
                })
                .collect();
            if !summary.choices.is_empty() {
                summary.correction_effect = Some(GameReviewEffect::ChoiceRequired);
            }
            let id = selection.map_or_else(
                || comparison_id.clone(),
                |selection| hash(&json!([comparison_id, selection.ply, selection.target])),
            );
            comparisons.push(ComparedGame {
                game,
                id,
                summary,
                observations,
                failures,
                resolved_choice: selection
                    .map(|selection| (comparison_id, selection.ply, selection.position.clone())),
            });
        }
        for compared in &comparisons {
            report.matches += compared.summary.matches;
            report.deviations += compared.summary.deviations;
            report.results.push(GameResult {
                comparison_id: Some(compared.id.clone()),
                summary: compared.summary.clone(),
                effect: compared
                    .summary
                    .correction_effect
                    .unwrap_or(GameReviewEffect::RecordedOnly),
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

    fn after_game_reset(
        &self,
        cutoff: Option<u64>,
        game: &GameRecord,
        played: &PlayedGame,
    ) -> bool {
        let Some(cutoff) = cutoff else {
            return true;
        };
        if let Some(start) = game.start_ms {
            return start > cutoff;
        }
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        parsed_game_date(played).is_some_and(|date| {
            date.signed_duration_since(epoch).num_days()
                >= i64::from(self.components.clock.day_at(cutoff).0)
        })
    }

    // Resolve all occurrences before projecting any report or Learning effect.
    // Store rechecks these effective times against concurrent writes at commit.
    fn resolve_pgn_timing(
        &self,
        dataset: &DatasetId,
        games: &mut [Result<(GameRecord, PlayedGame), String>],
    ) -> Result<(), AppError> {
        let mut timings = BTreeMap::<String, PgnTiming>::new();
        for (game, _) in games.iter().filter_map(|record| record.as_ref().ok()) {
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
        for (game, _) in games.iter_mut().filter_map(|record| record.as_mut().ok()) {
            if game.source == "pgn"
                && let Some(timing) = timings.get(&game.id)
            {
                game.end_ms = timing.end_ms;
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

fn parsed_game_date(game: &PlayedGame) -> Option<NaiveDate> {
    ["UTCDate", "Date"]
        .into_iter()
        .find_map(|tag| NaiveDate::parse_from_str(game.tags.get(tag)?, "%Y.%m.%d").ok())
}

fn parse_stored_game(mut game: GameRecord) -> Result<(GameRecord, PlayedGame), String> {
    let mut parsed = parse_played_games(&game.pgn).map_err(|error| error.to_string())?;
    if parsed.len() != 1 {
        return Err("Expected one stored completed game.".into());
    }
    let played = parsed.pop().unwrap().map_err(|error| error.to_string())?;
    if game.mainline_signature.is_empty() && game.start_ms.is_none() {
        game.mainline_signature = mainline_signature(&played, &game.white, &game.black);
    }
    Ok((game, played))
}

fn hash(value: &Value) -> String {
    blake3::hash(value.to_string().as_bytes())
        .to_hex()
        .to_string()
}

#[derive(serde::Deserialize, serde::Serialize)]
pub struct SelectedCorrection {
    pub comparison_id: String,
    pub ply: usize,
    pub target: TargetId,
    pub position: String,
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
    let date_tag = date
        .and_then(|date| NaiveDate::parse_from_str(date, "%Y.%m.%d").ok())
        .map_or_else(String::new, |date| {
            format!("[Date \"{}\"]\n", date.format("%Y.%m.%d"))
        });
    let pgn = format!(
        "[White {}]\n[Black {}]\n[Result {}]\n{setup}{date_tag}\n{}\n",
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
        played_date_ms: parsed_game_date(game)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .and_then(|date| date.and_utc().timestamp_millis().try_into().ok()),
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
