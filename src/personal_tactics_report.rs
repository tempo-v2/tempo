//! Learner-relative Games reports. No read operation issues or grades a card.
use crate::AppError;
use crate::application::tactical_analysis::{GAME_TACTICS_REPORT, TacticalDetails};
use chrono::{DateTime, Datelike, NaiveDate, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tempo_chess::{
    GameTimeControl, LearnerColor, LearnerGameResult, Motif, PositionKey, TacticalOutcome,
    game_report_facts, structural_neighbors,
};
use tempo_store::{SqliteStore, TacticalDecisionRow};
use tempo_types::DatasetId;

const PAGE: usize = 30;

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

#[derive(Default, Deserialize)]
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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PositionRequest {
    pub(crate) dataset_id: DatasetId,
    id: u64,
}

pub(crate) fn read_position(
    store: &SqliteStore,
    version: &str,
    request: PositionRequest,
) -> Result<Value, AppError> {
    let details = position(store, version, &request.dataset_id, request.id)?;
    Ok(json!({"schema_version":2,"dataset_id":request.dataset_id,"detail":details}))
}

fn position(
    store: &SqliteStore,
    version: &str,
    dataset: &DatasetId,
    id: u64,
) -> Result<Value, AppError> {
    let (encoded, request, fulfilled) = store
        .tactical_details(dataset, id, version)?
        .ok_or_else(|| AppError::new("unknown_position", "This finding has changed."))?;
    let details: TacticalDetails = serde_json::from_str(&encoded)
        .map_err(|e| crate::app_error::domain("invalid_tactical_history", e))?;
    Ok(json!({"id":id,"best_moves":details.assessment.best_moves(),
        "details":details,"request_id":request,"fulfilled":fulfilled}))
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

fn chances(rows: &[&TacticalDecisionRow], motif: Option<Motif>) -> Chances {
    let mut counts = Chances::default();
    for row in rows {
        let relevant = |(kind, uci): &(Motif, String)| {
            motif.is_none_or(|wanted| wanted == *kind) && row.accepted.contains(uci)
        };
        if !row.candidates.iter().any(relevant) {
            continue;
        }
        if motif.is_some() && row.outcome == TacticalOutcome::UnclearMiss {
            counts.unclear = true;
            continue;
        }
        counts.offered += 1;
        counts.missed += usize::from(matches!(
            row.outcome,
            TacticalOutcome::MissedTactic | TacticalOutcome::UnclearMiss
        ));
    }
    counts
}

fn loss_comparison(
    games: &[&Game],
    per_game: &BTreeMap<(&str, &str), Vec<&TacticalDecisionRow>>,
    motif: Option<Motif>,
) -> Value {
    let mut groups = [Results::default(); 2];
    let mut coverage = 0;
    let mut opportunity_counts = [[Results::default(); 2]; 3];
    for game in games.iter().filter(|g| g.fully_analyzed) {
        coverage += 1;
        let rows = per_game
            .get(&(game.source.as_str(), game.id.as_str()))
            .map(Vec::as_slice)
            .unwrap_or_default();
        let evidence = chances(rows, motif);
        if evidence.offered == 0 || evidence.unclear && evidence.missed == 0 {
            continue;
        }
        let group = usize::from(evidence.missed != 0);
        groups[group].add(game.result);
        opportunity_counts[evidence.offered.min(3) - 1][group].add(game.result);
    }
    json!({"with_miss":groups[1],"without_miss":groups[0],"analyzed":coverage,"opportunity_counts":opportunity_counts})
}

pub(crate) fn read(
    store: &SqliteStore,
    version: &str,
    request: ReportRequest,
) -> Result<Value, AppError> {
    let dataset = &request.dataset_id;
    let rows = store.tactical_archive(dataset, version)?;
    let mut ends = rows
        .iter()
        .filter_map(|game| game.end_ms)
        .collect::<Vec<_>>();
    ends.sort_unstable();
    let zone = chrono_tz::America::New_York;
    let mut games = BTreeMap::new();
    for game in rows {
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
        let summary = Game {
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
            prior_today: prior
                .zip(midnight)
                .map(|(before, midnight)| before - ends.partition_point(|end| *end < midnight)),
            prior_hour: prior.zip(game.start_ms).map(|(before, start)| {
                before - ends.partition_point(|end| *end < start.saturating_sub(3_600_000))
            }),
            fully_analyzed: game.fully_analyzed,
        };
        games.insert((summary.source.clone(), summary.id.clone()), summary);
    }
    let mut decisions = store.tactical_decisions(dataset, GAME_TACTICS_REPORT, version)?;
    // The report runs off the write owner. A game import can change the selected
    // learner between the reads, so bind each finding to the captured perspective.
    decisions.retain(|row| {
        games
            .get(&(row.source.clone(), row.game_id.clone()))
            .is_some_and(|game| game.learner == row.learner)
    });
    let mut per_game: BTreeMap<(&str, &str), Vec<_>> = BTreeMap::new();
    for row in &decisions {
        per_game
            .entry((&row.source, &row.game_id))
            .or_default()
            .push(row);
    }
    let selected: Vec<_> = games
        .values()
        .filter(|g| request.filters.includes(g))
        .collect();
    let selected_keys: BTreeSet<_> = selected
        .iter()
        .map(|g| (g.source.as_str(), g.id.as_str()))
        .collect();
    let findings: Vec<_> = decisions
        .iter()
        .filter(|row| {
            selected_keys.contains(&(row.source.as_str(), row.game_id.as_str()))
                && request
                    .filters
                    .motif
                    .is_none_or(|motif| row.candidates.iter().any(|(kind, _)| *kind == motif))
        })
        .collect();
    let comparison = loss_comparison(&selected, &per_game, request.filters.motif);
    let pattern = |daily| {
        let bounds = if daily {
            [0, 10, 20, 40]
        } else {
            [0, 5, 10, 20]
        };
        let mut report = bounds.map(|min| LoadBand {
            min,
            ..LoadBand::default()
        });
        for g in &selected {
            let amount = if daily { g.prior_today } else { g.prior_hour };
            let Some(amount) = amount else { continue };
            let entry = &mut report[bounds.partition_point(|&min| min <= amount) - 1];
            entry.results.add(g.result);
            if let Some(day) = g.local_day {
                entry.days.insert(day);
            }
            if g.fully_analyzed {
                let rows = per_game
                    .get(&(g.source.as_str(), g.id.as_str()))
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let evidence = chances(rows, request.filters.motif);
                if !evidence.unclear {
                    entry.opportunities += evidence.offered;
                    entry.misses += evidence.missed;
                }
            }
        }
        let possible_break = find_break(&selected, daily, bounds);
        json!({"bands":report.iter().map(|band| json!({"min":band.min,"results":band.results,"opportunities":band.opportunities,"misses":band.misses,"days":band.days.len()})).collect::<Vec<_>>(),"possible_break":possible_break})
    };
    let peers = if let Some(anchor) = request.anchor {
        let reference = decisions
            .iter()
            .find(|row| row.id == anchor)
            .ok_or_else(|| {
                AppError::new("unknown_position", "The selected position is unavailable.")
            })?;
        let mut positions = BTreeMap::new();
        for row in &findings {
            positions
                .entry(PositionKey(row.position.clone()))
                .or_insert(*row);
        }
        let ranked = structural_neighbors(
            &format!("{} 0 1", reference.position),
            positions.keys(),
            PAGE,
        )
        .map_err(|e| crate::app_error::domain("invalid_position", e))?;
        Some(ranked.iter().map(|key| positions[key]).collect::<Vec<_>>())
    } else {
        None
    };
    let mut page: Vec<_> = match peers {
        Some(peers) => peers,
        None => findings
            .iter()
            .copied()
            .filter(|row| request.before.is_none_or(|before| row.id < before))
            .take(PAGE + 1)
            .collect(),
    };
    let next_before = (page.len() > PAGE).then(|| page[PAGE - 1].id);
    page.truncate(PAGE);
    let detail = page
        .first()
        .map(|row| position(store, version, dataset, row.id))
        .transpose()?;
    let entries: Vec<_> = page.into_iter().map(|row| {
        let motifs: BTreeSet<_> = row.candidates.iter().map(|(kind,_)| *kind).collect();
        json!({"id":row.id,"ply":row.ply,"outcome":row.outcome,"motifs":motifs,"game":games[&(row.source.clone(),row.game_id.clone())]})
    }).collect();
    let clocks: BTreeSet<_> = games.values().filter_map(|g| g.clock).collect();
    let learner_bands: BTreeSet<_> = games
        .values()
        .filter_map(|g| g.learner_rating.map(rating_band))
        .collect();
    let opponent_bands: BTreeSet<_> = games
        .values()
        .filter_map(|g| g.opponent_rating.map(rating_band))
        .collect();
    let sources: BTreeSet<_> = games.values().map(|g| &g.source).collect();
    Ok(
        json!({"schema_version":2,"dataset_id":dataset,"games":entries,"detail":detail,"next_before":next_before,
        "loss_comparison":comparison,"prior_today":pattern(true),"prior_hour":pattern(false),"filtered_games":selected.len(),
        "archive_games":games.len(),"clocks":clocks,"learner_bands":learner_bands,"opponent_bands":opponent_bands,"sources":sources}),
    )
}

#[derive(Default)]
struct LoadBand {
    min: usize,
    results: Results,
    opportunities: usize,
    misses: usize,
    days: BTreeSet<NaiveDate>,
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
