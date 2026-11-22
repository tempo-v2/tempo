//! Learner-relative Games reports. No read operation issues or grades a card.
use crate::AppError;
use crate::application::tactical_analysis::{GAME_TACTICS_REPORT, TacticalDetails};
use crate::diagnostics::Span;
use chrono::{DateTime, Datelike, NaiveDate, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use tempo_chess::{
    GameTimeControl, LearnerColor, LearnerGameResult, Motif, PositionKey, PositionStructure,
    TacticalOutcome, game_report_facts,
};
use tempo_store::{
    SqliteStore, TacticalArchiveCoverage, TacticalArchiveMetadata, TacticalDecisionRow,
    TacticalReportGeneration,
};
use tempo_types::DatasetId;

const PAGE: usize = 30;
// Keep one compact archive and evidence generation. The number of filters and
// anchors retained is fixed. Each filter holds at most one index per finding,
// and each anchor holds at most one page. Raw PGNs are never cached.
const FILTER_CACHE: usize = 4;
const ANCHOR_CACHE: usize = 4;

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Speed {
    Bullet,
    Blitz,
    Rapid,
    Classical,
}
fn speed(clock: GameTimeControl) -> Speed {
    match clock.estimated_seconds() {
        0..=179 => Speed::Bullet,
        180..=479 => Speed::Blitz,
        480..=1499 => Speed::Rapid,
        _ => Speed::Classical,
    }
}

#[derive(Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct Filters {
    motif: Option<Motif>,
    result: Option<LearnerGameResult>,
    speed: Option<Speed>,
    clock: Option<GameTimeControl>,
    source: Option<String>,
    learner_rating: Option<u32>,
    opponent_rating: Option<u32>,
    weekday: Option<u32>,
    start_hour: Option<u32>,
    prior_today: Option<[usize; 2]>,
    prior_hour: Option<[usize; 2]>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReportRequest {
    pub(crate) dataset_id: DatasetId,
    #[serde(default)]
    filters: Filters,
    before: Option<u64>,
    anchor: Option<u64>,
    report_generation: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PositionRequest {
    pub(crate) dataset_id: DatasetId,
    id: u64,
    report_generation: Option<String>,
}

/// A report generation is scoped to both the dataset and accepted analysis policy.
pub(crate) fn report_generation(version: &str, evidence: u64) -> String {
    format!("{version}:{evidence}")
}

fn check_generation(expected: Option<&str>, actual: &str) -> Result<(), AppError> {
    if expected.is_some_and(|expected| expected != actual) {
        return Err(AppError::new(
            "report_changed",
            "This report snapshot has changed. Refresh the report to continue.",
        ));
    }
    Ok(())
}

pub(crate) fn read_position(
    store: &SqliteStore,
    version: &str,
    request: PositionRequest,
) -> Result<Value, AppError> {
    store.with_tactical_report_snapshot(&request.dataset_id, |read, generation| {
        let stamp = report_generation(version, generation.evidence);
        check_generation(request.report_generation.as_deref(), &stamp)?;
        let details = position(read, version, &request.dataset_id, request.id)?;
        Ok(json!({"schema_version":2,"dataset_id":request.dataset_id,"report_generation":stamp,"detail":details}))
    })
}

fn position(
    store: &SqliteStore,
    version: &str,
    dataset: &DatasetId,
    id: u64,
) -> Result<Value, AppError> {
    let _trace = Span::new("tactics.report.position");
    let stored = store
        .tactical_details(dataset, id, version)?
        .ok_or_else(|| AppError::new("unknown_position", "This finding has changed."))?;
    let details: TacticalDetails = serde_json::from_str(&stored.evidence_json)
        .map_err(|e| crate::app_error::domain("invalid_tactical_history", e))?;
    Ok(json!({"id":id,"best_moves":details.assessment.best_moves(),
        "details":details,"training":stored.training}))
}

#[derive(Clone, Serialize)]
struct Game {
    source: String,
    id: String,
    learner: String,
    result: LearnerGameResult,
    clock: Option<GameTimeControl>,
    learner_rating: Option<u32>,
    opponent_rating: Option<u32>,
    #[serde(skip)]
    local_day: Option<NaiveDate>,
    weekday: Option<u32>,
    hour: Option<u32>,
    prior_today: Option<usize>,
    prior_hour: Option<usize>,
    fully_analyzed: bool,
}

fn within(range: Option<[usize; 2]>, amount: Option<usize>) -> bool {
    range.is_none_or(|[min, max]| amount.is_some_and(|n| (min..=max).contains(&n)))
}
fn rating_band(rating: u32) -> u32 {
    rating / 50 * 50
}

impl Filters {
    fn includes(&self, game: &Game) -> bool {
        self.result.is_none_or(|result| result == game.result)
            && self
                .speed
                .is_none_or(|wanted| game.clock.map(speed) == Some(wanted))
            && self.clock.is_none_or(|clock| game.clock == Some(clock))
            && self
                .source
                .as_ref()
                .is_none_or(|source| source == &game.source)
            && self
                .learner_rating
                .is_none_or(|n| game.learner_rating.map(rating_band) == Some(n))
            && self
                .opponent_rating
                .is_none_or(|n| game.opponent_rating.map(rating_band) == Some(n))
            && self.weekday.is_none_or(|day| game.weekday == Some(day))
            && self.start_hour.is_none_or(|hour| game.hour == Some(hour))
            && within(self.prior_today, game.prior_today)
            && within(self.prior_hour, game.prior_hour)
    }
}

#[derive(Clone, Copy, Default, Serialize)]
struct Results {
    wins: usize,
    draws: usize,
    losses: usize,
}
impl Results {
    fn add(&mut self, result: LearnerGameResult) {
        match result {
            LearnerGameResult::Win => self.wins += 1,
            LearnerGameResult::Draw => self.draws += 1,
            LearnerGameResult::Loss => self.losses += 1,
            LearnerGameResult::Unknown => {}
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Chances {
    offered: usize,
    missed: usize,
    unclear: bool,
}

impl Chances {
    fn observe(&mut self, row: &Decision, motif: Option<Motif>) {
        if !row
            .accepted_motifs
            .iter()
            .any(|&kind| motif.is_none_or(|wanted| wanted == kind))
        {
            return;
        }
        if motif.is_some() && row.outcome == TacticalOutcome::UnclearMiss {
            self.unclear = true;
            return;
        }
        self.offered += 1;
        self.missed += usize::from(matches!(
            row.outcome,
            TacticalOutcome::MissedTactic | TacticalOutcome::UnclearMiss
        ));
    }
}

type SelectedGame<'a> = (&'a Game, Chances);

fn loss_comparison(games: &[SelectedGame<'_>]) -> Value {
    let mut groups = [Results::default(); 2];
    let mut coverage = 0;
    let mut opportunity_counts = [[Results::default(); 2]; 3];
    for &(game, evidence) in games.iter().filter(|(game, _)| game.fully_analyzed) {
        coverage += 1;
        if evidence.offered == 0 || evidence.unclear && evidence.missed == 0 {
            continue;
        }
        let group = usize::from(evidence.missed != 0);
        groups[group].add(game.result);
        opportunity_counts[evidence.offered.min(3) - 1][group].add(game.result);
    }
    json!({"with_miss":groups[1],"without_miss":groups[0],"analyzed":coverage,"opportunity_counts":opportunity_counts})
}

/// The existing single report worker owns this cache. Reads drive reconstruction
/// after startup or a changed generation. Normal status polling never enters it.
#[derive(Default)]
pub(crate) struct ReportCache {
    archive: Option<Archive>,
    evidence: Option<Evidence>,
    queries: VecDeque<Query>,
    #[cfg(test)]
    pub(crate) work: WorkCounts,
}

#[cfg(test)]
#[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
pub(crate) struct WorkCounts {
    pub(crate) pgns: usize,
    pub(crate) decisions: usize,
    pub(crate) aggregates: usize,
    pub(crate) similarity: usize,
    pub(crate) page_rows: usize,
}

struct Captured {
    generation: TacticalReportGeneration,
    metadata: Option<Vec<TacticalArchiveMetadata>>,
    evidence: Option<(Vec<TacticalArchiveCoverage>, Vec<TacticalDecisionRow>)>,
}

impl ReportCache {
    fn capture(
        &self,
        store: &SqliteStore,
        version: &str,
        request: &ReportRequest,
    ) -> Result<Captured, AppError> {
        let mut trace = Span::new("tactics.report.capture");
        let dataset = &request.dataset_id;
        let captured = store.with_tactical_report_snapshot(dataset, |read, generation| {
            check_generation(
                request.report_generation.as_deref(),
                &report_generation(version, generation.evidence),
            )?;
            let metadata = self
                .archive
                .as_ref()
                .is_none_or(|archive| {
                    &archive.dataset != dataset || archive.generation != generation.archive
                })
                .then(|| read.tactical_archive_metadata(dataset))
                .transpose()?;
            let evidence = if metadata.is_some()
                || self.evidence.as_ref().is_none_or(|cached| {
                    cached.generation != generation.evidence || cached.version != version
                }) {
                Some((
                    read.tactical_archive_coverage(dataset, GAME_TACTICS_REPORT, version)?,
                    read.tactical_decisions(dataset, GAME_TACTICS_REPORT, version)?,
                ))
            } else {
                None
            };
            Ok(Captured {
                generation,
                metadata,
                evidence,
            })
        });
        trace.set_outcome(match &captured {
            Ok(input) if input.metadata.is_some() => "archive",
            Ok(input) if input.evidence.is_some() => "evidence",
            Ok(_) => "reused",
            Err(_) => "error",
        });
        captured
    }

    pub(crate) fn read(
        &mut self,
        store: &SqliteStore,
        version: &str,
        request: ReportRequest,
    ) -> Result<Value, AppError> {
        let Captured {
            generation,
            metadata,
            evidence,
        } = self.capture(store, version, &request)?;
        let dataset = &request.dataset_id;
        if let Some(rows) = metadata {
            #[cfg(test)]
            {
                self.work.pgns += rows.len();
            }
            self.archive = Some(Archive::build(dataset.clone(), generation.archive, rows));
        }
        let archive = self.archive.as_mut().expect("captured archive");
        if let Some((coverage, rows)) = evidence {
            #[cfg(test)]
            {
                self.work.decisions += rows.len();
            }
            self.evidence = Some(Evidence::build(
                archive,
                generation.evidence,
                version,
                coverage,
                rows,
            ));
            self.queries.clear();
        }
        let evidence = self.evidence.as_ref().expect("captured report evidence");
        let cached = self
            .queries
            .iter()
            .position(|query| query.filters == request.filters);
        let query = {
            let mut trace = Span::new("tactics.report.aggregates");
            if let Some(index) = cached {
                trace.set_outcome("reused");
                self.queries.remove(index).expect("cached query")
            } else {
                trace.set_outcome("rebuilt");
                #[cfg(test)]
                {
                    self.work.aggregates += 1;
                }
                Query::build(archive, evidence, request.filters)
            }
        };
        self.queries.push_back(query);
        if self.queries.len() > FILTER_CACHE {
            self.queries.pop_front();
        }
        let query = self.queries.back_mut().expect("current query");
        let (page, next_before) = if let Some(anchor) = request.anchor {
            #[cfg(test)]
            if !query.similarities.iter().any(|(id, _)| *id == anchor) {
                self.work.similarity += 1;
            }
            (query.similar(anchor, evidence)?, None)
        } else {
            query.page(request.before, evidence)
        };
        #[cfg(test)]
        {
            self.work.page_rows += page.len();
        }
        let stamp = report_generation(version, generation.evidence);
        // Cache building runs outside SQLite. A short second read both checks
        // the facts still match and loads only the selected detail and its
        // current training link. A changed generation cannot give a mixed page.
        let detail = store.with_tactical_report_snapshot(dataset, |read, current| {
            check_generation(Some(&stamp), &report_generation(version, current.evidence))?;
            page.first()
                .map(|&index| position(read, version, dataset, evidence.rows[index].id))
                .transpose()
        })?;
        let entries: Vec<_> = {
            let mut trace = Span::new("tactics.report.page");
            trace.field("count", page.len());
            page.iter().map(|&index| {
                let row = &evidence.rows[index];
                json!({"id":row.id,"ply":row.ply,"outcome":row.outcome,"motifs":row.motifs,"game":archive.games[row.game]})
            }).collect()
        };
        let aggregate = &query.aggregate;
        Ok(
            json!({"schema_version":2,"dataset_id":dataset,"report_generation":stamp,"games":entries,
            "detail":detail,"next_before":next_before,"loss_comparison":aggregate.comparison,
            "simple_tactics":aggregate.simple_tactics,
            "prior_today":aggregate.prior_today,"prior_hour":aggregate.prior_hour,
            "filtered_games":aggregate.selected,"archive_games":archive.games.len(),"clocks":archive.clocks,
            "learner_bands":archive.learner_bands,"opponent_bands":archive.opponent_bands,"sources":archive.sources}),
        )
    }
}

#[cfg(test)]
pub(crate) fn read(
    store: &SqliteStore,
    version: &str,
    request: ReportRequest,
) -> Result<Value, AppError> {
    ReportCache::default().read(store, version, request)
}

struct Archive {
    dataset: DatasetId,
    generation: u64,
    games: Vec<Game>,
    game_index: BTreeMap<(String, String), usize>,
    clocks: BTreeSet<GameTimeControl>,
    learner_bands: BTreeSet<u32>,
    opponent_bands: BTreeSet<u32>,
    sources: BTreeSet<String>,
}

impl Archive {
    fn build(dataset: DatasetId, generation: u64, mut rows: Vec<TacticalArchiveMetadata>) -> Self {
        let mut trace = Span::new("tactics.report.archive_facts");
        trace.field("count", rows.len());
        rows.sort_unstable_by(|left, right| {
            (&left.source, &left.id).cmp(&(&right.source, &right.id))
        });
        let mut ends = rows
            .iter()
            .filter_map(|game| game.end_ms)
            .collect::<Vec<_>>();
        ends.sort_unstable();
        let zone = chrono_tz::America::New_York;
        let games: Vec<_> = rows
            .into_iter()
            .map(|game| {
                let local = game
                    .start_ms
                    .and_then(DateTime::from_timestamp_millis)
                    .map(|start| start.with_timezone(&zone));
                let prior = game
                    .start_ms
                    .map(|start| ends.partition_point(|end| *end < start));
                let midnight = local
                    .as_ref()
                    .and_then(|start| start.date_naive().and_hms_opt(0, 0, 0))
                    .and_then(|time| time.and_local_timezone(zone).single())
                    .map(|start| start.timestamp_millis());
                let result = game_report_facts(
                    &game.pgn,
                    if game.white.to_lowercase() == game.learner {
                        LearnerColor::White
                    } else {
                        LearnerColor::Black
                    },
                );
                Game {
                    source: game.source,
                    id: game.id,
                    learner: game.learner,
                    result: result.result,
                    clock: result.time_control,
                    learner_rating: result.learner_rating,
                    opponent_rating: result.opponent_rating,
                    local_day: local.as_ref().map(|d| d.date_naive()),
                    weekday: local.as_ref().map(|d| d.weekday().num_days_from_monday()),
                    hour: local.as_ref().map(|d| d.hour()),
                    prior_today: prior.zip(midnight).map(|(before, midnight)| {
                        before - ends.partition_point(|end| *end < midnight)
                    }),
                    prior_hour: prior.zip(game.start_ms).map(|(before, start)| {
                        before - ends.partition_point(|end| *end < start.saturating_sub(3_600_000))
                    }),
                    fully_analyzed: false,
                }
            })
            .collect();
        Self {
            dataset,
            generation,
            clocks: games.iter().filter_map(|g| g.clock).collect(),
            learner_bands: games
                .iter()
                .filter_map(|g| g.learner_rating.map(rating_band))
                .collect(),
            opponent_bands: games
                .iter()
                .filter_map(|g| g.opponent_rating.map(rating_band))
                .collect(),
            sources: games.iter().map(|g| g.source.clone()).collect(),
            game_index: games
                .iter()
                .enumerate()
                .map(|(index, g)| ((g.source.clone(), g.id.clone()), index))
                .collect(),
            games,
        }
    }
}

struct Descriptor {
    key: PositionKey,
    structure: Result<PositionStructure, tempo_chess::ChessError>,
}

impl Descriptor {
    fn structure(&self) -> Result<&PositionStructure, AppError> {
        self.structure
            .as_ref()
            .map_err(|error| crate::app_error::domain("invalid_position", error))
    }
}

struct Decision {
    id: u64,
    ply: u32,
    outcome: TacticalOutcome,
    position: usize,
    game: usize,
    motifs: BTreeSet<Motif>,
    accepted_motifs: BTreeSet<Motif>,
    recognition: Option<tempo_chess::TacticRecognition>,
}

struct Evidence {
    generation: u64,
    version: String,
    /// Descending finding ID is the report's stable keyset order.
    rows: Vec<Decision>,
    positions: Vec<Descriptor>,
}

impl Evidence {
    fn build(
        archive: &mut Archive,
        generation: u64,
        version: &str,
        coverage: Vec<TacticalArchiveCoverage>,
        rows: Vec<TacticalDecisionRow>,
    ) -> Self {
        let mut trace = Span::new("tactics.report.decision_facts");
        trace.field("count", rows.len());
        for game in &mut archive.games {
            game.fully_analyzed = false;
        }
        for row in coverage {
            if let Some(&index) = archive.game_index.get(&(row.source, row.id)) {
                let game = &mut archive.games[index];
                if game.learner == row.learner {
                    game.fully_analyzed = row.fully_analyzed;
                }
            }
        }
        let mut descriptors = Vec::new();
        let mut positions = BTreeMap::new();
        let rows = rows
            .into_iter()
            .filter_map(|row| {
                let &game = archive.game_index.get(&(row.source, row.game_id))?;
                if archive.games[game].learner != row.learner {
                    return None;
                }
                let key = PositionKey(row.position);
                let position = *positions.entry(key.clone()).or_insert_with(|| {
                    let index = descriptors.len();
                    descriptors.push(Descriptor {
                        structure: PositionStructure::from_key(&key),
                        key,
                    });
                    index
                });
                let motifs = row.candidates.iter().map(|&(kind, _)| kind).collect();
                let accepted_motifs = row
                    .candidates
                    .into_iter()
                    .filter_map(|(kind, uci)| row.accepted.contains(&uci).then_some(kind))
                    .collect();
                Some(Decision {
                    id: row.id,
                    ply: row.ply,
                    outcome: row.outcome,
                    position,
                    game,
                    motifs,
                    accepted_motifs,
                    recognition: row.recognition,
                })
            })
            .collect();
        Self {
            generation,
            version: version.to_owned(),
            rows,
            positions: descriptors,
        }
    }
}

#[derive(Default, Serialize)]
struct SimpleTactics {
    found: usize,
    missed: usize,
    alternatives: usize,
    unclear: usize,
    awaiting_verification: usize,
}

impl SimpleTactics {
    fn observe(&mut self, row: &Decision, motif: Option<Motif>) {
        let Some(verified) = &row.recognition else {
            if row
                .accepted_motifs
                .iter()
                .any(|kind| motif.is_none_or(|m| m == *kind))
            {
                self.awaiting_verification += 1;
            }
            return;
        };
        if motif.is_some_and(|kind| !verified.motifs.contains(&kind)) {
            return;
        }
        match verified.outcome {
            TacticalOutcome::PlayedTactic if motif.is_some() && motif != verified.played_motif => {
                self.alternatives += 1;
            }
            TacticalOutcome::PlayedTactic => self.found += 1,
            TacticalOutcome::MissedTactic => self.missed += 1,
            TacticalOutcome::GoodAlternative => self.alternatives += 1,
            TacticalOutcome::UnclearMiss
            | TacticalOutcome::UnclearPlay
            | TacticalOutcome::Uncertain => self.unclear += 1,
            _ => {}
        }
    }
}

struct Aggregate {
    simple_tactics: SimpleTactics,
    selected: usize,
    comparison: Value,
    prior_today: Value,
    prior_hour: Value,
}

struct Query {
    filters: Filters,
    findings: Vec<usize>,
    aggregate: Aggregate,
    similarities: VecDeque<(u64, Vec<usize>)>,
}

impl Query {
    fn build(archive: &Archive, evidence: &Evidence, filters: Filters) -> Self {
        let selected: Vec<_> = archive.games.iter().map(|g| filters.includes(g)).collect();
        let mut chances = vec![Chances::default(); archive.games.len()];
        let mut findings = Vec::new();
        let mut simple_tactics = SimpleTactics::default();
        for (index, row) in evidence
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| selected[row.game])
        {
            chances[row.game].observe(row, filters.motif);
            simple_tactics.observe(row, filters.motif);
            if filters
                .motif
                .is_none_or(|motif| row.motifs.contains(&motif))
            {
                findings.push(index);
            }
        }
        let games: Vec<_> = archive
            .games
            .iter()
            .zip(chances)
            .zip(selected)
            .filter_map(|(row, selected)| selected.then_some(row))
            .collect();
        let selected_games: Vec<_> = games.iter().map(|(game, _)| *game).collect();
        let pattern = |daily| {
            let bounds = if daily {
                [0, 10, 20, 40]
            } else {
                [0, 5, 10, 20]
            };
            let report = playing_load(&games, daily, bounds);
            let possible_break = find_break(&selected_games, daily, bounds);
            json!({"bands":report.iter().map(|band| json!({"min":band.min,"results":band.results,
                "opportunities":band.opportunities,"misses":band.misses,"days":band.days.len()}))
                .collect::<Vec<_>>(),"possible_break":possible_break})
        };
        let aggregate = Aggregate {
            simple_tactics,
            selected: games.len(),
            comparison: loss_comparison(&games),
            prior_today: pattern(true),
            prior_hour: pattern(false),
        };
        Self {
            filters,
            findings,
            aggregate,
            similarities: VecDeque::new(),
        }
    }

    fn page(&self, before: Option<u64>, evidence: &Evidence) -> (&[usize], Option<u64>) {
        let start = before.map_or(0, |before| {
            self.findings
                .partition_point(|&index| evidence.rows[index].id >= before)
        });
        let end = (start + PAGE).min(self.findings.len());
        let next = (end < self.findings.len()).then(|| evidence.rows[self.findings[end - 1]].id);
        (&self.findings[start..end], next)
    }

    fn similar(&mut self, anchor: u64, evidence: &Evidence) -> Result<&[usize], AppError> {
        let mut trace = Span::new("tactics.report.similarity");
        let cached = self.similarities.iter().position(|(id, _)| *id == anchor);
        let ranked = if let Some(index) = cached {
            trace.set_outcome("reused");
            self.similarities
                .remove(index)
                .expect("cached similarity")
                .1
        } else {
            trace.field("count", self.findings.len());
            let result = self.rank(anchor, evidence);
            trace.set_outcome(if result.is_ok() { "rebuilt" } else { "error" });
            result?
        };
        self.similarities.push_back((anchor, ranked));
        if self.similarities.len() > ANCHOR_CACHE {
            self.similarities.pop_front();
        }
        Ok(&self.similarities.back().expect("current similarity").1)
    }

    fn rank(&self, anchor: u64, evidence: &Evidence) -> Result<Vec<usize>, AppError> {
        let anchor = evidence
            .rows
            .binary_search_by(|row| anchor.cmp(&row.id))
            .map_err(|_| {
                AppError::new("unknown_position", "The selected position is unavailable.")
            })?;
        let reference = evidence.positions[evidence.rows[anchor].position].structure()?;
        let mut seen = vec![false; evidence.positions.len()];
        let mut nearest = BTreeMap::new();
        for &index in &self.findings {
            let position = evidence.rows[index].position;
            if std::mem::replace(&mut seen[position], true) {
                continue;
            }
            let candidate = &evidence.positions[position];
            if let Some(rank) = reference.rank(&candidate.key, candidate.structure()?) {
                nearest.insert(rank, index);
                if nearest.len() > PAGE {
                    nearest.pop_last();
                }
            }
        }
        Ok(nearest.into_values().collect())
    }
}

#[derive(Default)]
struct LoadBand {
    min: usize,
    results: Results,
    opportunities: usize,
    misses: usize,
    days: BTreeSet<NaiveDate>,
}

fn playing_load(games: &[SelectedGame<'_>], daily: bool, bounds: [usize; 4]) -> [LoadBand; 4] {
    let mut report = bounds.map(|min| LoadBand {
        min,
        ..LoadBand::default()
    });
    for &(g, evidence) in games {
        let amount = if daily { g.prior_today } else { g.prior_hour };
        let Some(amount) = amount else { continue };
        let entry = &mut report[bounds.partition_point(|&min| min <= amount) - 1];
        entry.results.add(g.result);
        if let Some(day) = g.local_day {
            entry.days.insert(day);
        }
        if g.fully_analyzed {
            entry.opportunities += evidence.offered;
            entry.misses += evidence.missed;
        }
    }
    report
}

type Stratum<'a> = (&'a str, GameTimeControl, i64);
type DailyCounts<'a> = BTreeMap<Stratum<'a>, [[usize; 2]; 2]>;

fn find_break(games: &[&Game], daily: bool, bins: [usize; 4]) -> Option<Value> {
    for min in bins.into_iter().skip(1) {
        let mut days = BTreeMap::<NaiveDate, DailyCounts<'_>>::new();
        for game in games {
            let (Some(day), Some(clock), Some(ours), Some(theirs), Some(count)) = (
                game.local_day,
                game.clock,
                game.learner_rating,
                game.opponent_rating,
                if daily {
                    game.prior_today
                } else {
                    game.prior_hour
                },
            ) else {
                continue;
            };
            if game.result == LearnerGameResult::Unknown {
                continue;
            }
            let group = if count < bins[1] {
                0
            } else if count >= min {
                1
            } else {
                continue;
            };
            let gap = (i64::from(theirs) - i64::from(ours)).div_euclid(50);
            let cell = days
                .entry(day)
                .or_default()
                .entry((&game.source, clock, gap))
                .or_default();
            cell[group][0] += 1;
            cell[group][1] += usize::from(game.result == LearnerGameResult::Loss);
        }
        if days.len() < 5 {
            continue;
        }
        let mut arms = BTreeMap::<Stratum<'_>, [bool; 2]>::new();
        for day in days.values() {
            for (&key, entries) in day {
                let matched = arms.entry(key).or_default();
                for (found, entry) in matched.iter_mut().zip(entries) {
                    *found |= entry[0] > 0;
                }
            }
        }
        for day in days.values_mut() {
            day.retain(|key, _| arms[key] == [true, true]);
        }
        let samples: Vec<_> = days.values().filter(|day| !day.is_empty()).collect();
        let (effect, counts) = difference(samples.iter().copied());
        if samples.len() < 5 || counts.iter().any(|&n| n < 30) || effect <= 0.0 {
            continue;
        }
        // Sample whole playing dates together, retaining the rating and clock strata.
        let mut seed = 1_u64;
        let mut distribution = Vec::new();
        for _ in 0..400 {
            let (value, coverage) = difference((0..samples.len()).map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                samples[((seed >> 32) as usize) % samples.len()]
            }));
            if coverage.contains(&0) {
                continue;
            }
            distribution.push(value);
        }
        if distribution.len() < 380 {
            continue;
        }
        distribution.sort_by(f64::total_cmp);
        let lower = distribution[distribution.len() / 40];
        if lower > 0.0 {
            return Some(
                json!({"min":min,"difference":effect,"lower":lower,"upper":distribution[distribution.len()*39/40],"counts":counts,"days":samples.len()}),
            );
        }
    }
    None
}

fn difference<'a, 'b>(days: impl Iterator<Item = &'b DailyCounts<'a>>) -> (f64, [usize; 2])
where
    'a: 'b,
{
    let mut all = DailyCounts::new();
    for day in days {
        for (&key, cells) in day {
            let combined = all.entry(key).or_default();
            for (total, from) in combined.iter_mut().zip(cells) {
                total[0] += from[0];
                total[1] += from[1];
            }
        }
    }
    let mut counts = [0, 0];
    let mut weighted = 0.0;
    for [[early, early_lost], [late, late_lost]] in all.into_values() {
        if early == 0 || late == 0 {
            continue;
        }
        counts[0] += early;
        counts[1] += late;
        weighted += (late_lost as f64 / late as f64 - early_lost as f64 / early as f64)
            * (early + late) as f64;
    }
    (weighted / (counts[0] + counts[1]).max(1) as f64, counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(day: u32, result: LearnerGameResult, prior: usize, clock: u32) -> Game {
        Game {
            source: "fixture".into(),
            id: String::new(),
            learner: "player".into(),
            result,
            clock: Some(GameTimeControl {
                seconds: clock,
                increment: 0,
            }),
            learner_rating: Some(1200),
            opponent_rating: Some(1245),
            local_day: NaiveDate::from_ymd_opt(2026, 11, day),
            weekday: None,
            hour: None,
            prior_today: Some(prior),
            prior_hour: Some(prior),
            fully_analyzed: true,
        }
    }

    #[test]
    fn ambiguous_turn_preserves_the_same_games_clear_motif_chance() {
        let game = sample(1, LearnerGameResult::Loss, 12, 60);
        let row = |id, outcome| Decision {
            id,
            ply: id as u32,
            outcome,
            game: 0,
            position: 0,
            motifs: BTreeSet::from([Motif::Fork]),
            accepted_motifs: BTreeSet::from([Motif::Fork]),
            recognition: None,
        };
        let clear = row(1, TacticalOutcome::MissedTactic);
        let ambiguous = row(2, TacticalOutcome::UnclearMiss);
        let counts = |motif| {
            let mut counts = Chances::default();
            counts.observe(&clear, motif);
            counts.observe(&ambiguous, motif);
            counts
        };
        let fork = [(&game, counts(Some(Motif::Fork)))];
        let bands = playing_load(&fork, true, [0, 10, 20, 40]);
        assert_eq!((bands[1].opportunities, bands[1].misses), (1, 1));
        let all = playing_load(&[(&game, counts(None))], true, [0, 10, 20, 40]);
        assert_eq!((all[1].opportunities, all[1].misses), (2, 2));
        let games = loss_comparison(&fork);
        assert_eq!(games["with_miss"]["losses"], 1);
    }

    #[test]
    fn a_break_comparison_needs_matching_clocks_and_support_across_playing_days() {
        let matching: Vec<_> = (1..=6)
            .flat_map(|day| {
                (0..6).flat_map(move |_| {
                    [
                        sample(day, LearnerGameResult::Win, 0, 60),
                        sample(day, LearnerGameResult::Loss, 12, 60),
                    ]
                })
            })
            .collect();
        let matched =
            find_break(&matching.iter().collect::<Vec<_>>(), true, [0, 10, 20, 40]).unwrap();
        assert_eq!(matched["min"], 10);
        assert_eq!(matched["days"], 6);
        assert_eq!(matched["counts"], json!([36, 36]));
        let different_clocks: Vec<_> = matching
            .into_iter()
            .map(|mut game| {
                if game.result == LearnerGameResult::Loss {
                    game.clock = Some(GameTimeControl {
                        seconds: 180,
                        increment: 2,
                    });
                }
                game
            })
            .collect();
        assert!(
            find_break(
                &different_clocks.iter().collect::<Vec<_>>(),
                true,
                [0, 10, 20, 40]
            )
            .is_none()
        );
    }
}
