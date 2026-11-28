use crate::{catalog, chess, gap_pgn};
use anyhow::{Context, Result, ensure};
use pgn_reader::shakmaty::Position;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub type Choices = BTreeMap<String, String>;

#[derive(Serialize)]
pub struct ChangedLine {
    name: String,
    before: String,
    after: Option<String>,
    stopped_at: Option<String>,
}

pub struct Replacement {
    pub line_revision: i64,
    pub impact: Vec<ChangedLine>,
    pub over_budget: bool,
    base_writes: usize,
    archived: Vec<String>,
    retired: Vec<String>,
    rewritten: Vec<catalog::OpeningPlan>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum PreviewUnavailable {
    WorkLimit,
    CyclicReplies,
}

impl PreviewUnavailable {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::WorkLimit => "work_limit",
            Self::CyclicReplies => "cyclic_replies",
        }
    }
}

impl std::fmt::Display for PreviewUnavailable {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(match self {
            Self::WorkLimit => "These saved continuations exceed the preview work limit. Clear staged repertoire changes to continue editing.",
            Self::CyclicReplies => "The saved continuations reach competing replies in different orders. Preview the replacements separately.",
        })
    }
}
impl std::error::Error for PreviewUnavailable {}

pub fn revision(db: &Connection, repertoire: &str) -> Result<i64> {
    Ok(db
        .query_row(
            "SELECT revision FROM opening_line_revision WHERE repertoire=?1",
            [repertoire],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

struct SavedLine {
    id: String,
    input: catalog::OpeningInput,
}

fn saved_lines(db: &Connection, repertoire: &str, color: &str) -> Result<Vec<SavedLine>> {
    let mut query = db.prepare(
        "SELECT id,name,route,start_at FROM authored_openings
         WHERE repertoire_id=?1 AND color=?2 ORDER BY id LIMIT 2001",
    )?;
    let mut rows = query.query((repertoire, color))?;
    let mut lines = Vec::new();
    let mut moves = 0;
    let mut expanded_work = 0;
    while let Some(row) = rows.next()? {
        let route: chess::Route = serde_json::from_str(&row.get::<_, String>(2)?)?;
        moves += route.moves.len();
        expanded_work += route.moves.len().pow(2);
        ensure!(
            lines.len() < 2000 && moves <= 100_000 && expanded_work <= 8_000_000,
            PreviewUnavailable::WorkLimit
        );
        lines.push(SavedLine {
            id: row.get(0)?,
            input: catalog::OpeningInput {
                name: row.get(1)?,
                color: color.into(),
                route,
                start_at: row.get(3)?,
            },
        });
    }
    Ok(lines)
}

#[derive(Default)]
struct Rewritten {
    moves: Vec<String>,
    decisions: Vec<ContinuationReply>,
    stopped_at: Option<String>,
}

struct ContinuationReply {
    ply: usize,
    position: String,
    movement: String,
    label: String,
}

enum SavedVersion {
    Active(String),
    Quarantined,
}

impl Rewritten {
    fn stop_before(&mut self, incompatible: impl Fn(&ContinuationReply) -> bool) {
        if let Some(decision) = self.decisions.iter().find(|reply| incompatible(reply)) {
            let ply = decision.ply;
            self.stopped_at = Some(format!(
                "{} would introduce another reply conflict.",
                decision.label
            ));
            self.moves.truncate(ply);
            self.decisions.retain(|reply| reply.ply < ply);
        }
    }
}

fn replay(
    input: &catalog::OpeningInput,
    selected: &Choices,
    removal: Option<(&str, &str)>,
) -> Result<Rewritten> {
    let mut result = Rewritten::default();
    let mut board = chess::position(&input.route.fen)?;
    let mut original_board = board.clone();
    let mut changed = false;
    for (ply, original) in input.route.moves.iter().enumerate() {
        let position = chess::position_id(&board);
        let original_move = chess::parse_move(&board, original).ok();
        let owned = ply >= input.start_at && chess::color_name(board.turn()) == input.color;
        if owned
            && removal.is_some_and(|(target, reply)| {
                position == target
                    && original_move.is_some_and(|movement| chess::uci(movement) == reply)
            })
        {
            result.stopped_at = Some(format!(
                "{} is removed.",
                gap_pgn::reply_text(&original_board, original)?
            ));
            break;
        }
        let movement = match selected.get(&position).filter(|_| owned) {
            Some(selected) => chess::parse_move(&board, selected).ok(),
            None => original_move,
        };
        let Some(movement) = movement else {
            result.stopped_at = Some(format!(
                "{} becomes illegal.",
                gap_pgn::reply_text(&original_board, original)?
            ));
            break;
        };
        let canonical = chess::uci(movement);
        changed |= Some(movement) != original_move;
        if owned && changed {
            result.decisions.push(ContinuationReply {
                ply,
                position,
                label: gap_pgn::reply_text(&board, &canonical)?,
                movement: canonical.clone(),
            });
        }
        let stored = if original_move == Some(movement) {
            original.clone()
        } else {
            canonical
        };
        result.moves.push(stored);
        board.play_unchecked(movement);
        let old = chess::parse_move(&original_board, original)?;
        original_board.play_unchecked(old);
    }
    Ok(result)
}

fn stop_at_known_conflict(
    continuation: &mut Rewritten,
    selected: &Choices,
    pasted: &Choices,
    current: &BTreeMap<String, SavedVersion>,
) {
    continuation.stop_before(|reply| {
        if let Some(expected) = selected
            .get(&reply.position)
            .or_else(|| pasted.get(&reply.position))
        {
            return expected != &reply.movement;
        }
        match current.get(&reply.position) {
            Some(SavedVersion::Active(version)) => {
                version != &chess::id(&["answer-v1", &reply.movement])
            }
            Some(SavedVersion::Quarantined) => true,
            None => false,
        }
    });
}

/// Resolve disagreements only when the competing replies can reach the
/// position without first crossing another unresolved disagreement.
fn stop_at_reachable_conflicts(
    continuations: &mut [Rewritten],
    is_new: impl Fn(&ContinuationReply) -> bool,
) -> Result<()> {
    let mut visits_left = 8_000_000usize;
    loop {
        let decisions: usize = continuations.iter().map(|line| line.decisions.len()).sum();
        visits_left = visits_left
            .checked_sub(3 * decisions)
            .ok_or(PreviewUnavailable::WorkLimit)?;
        let candidates = continuations
            .iter()
            .flat_map(|line| &line.decisions)
            .filter(|reply| is_new(reply));
        let potential = disagreements(candidates);
        if potential.is_empty() {
            return Ok(());
        }
        let first_reached = continuations.iter().flat_map(|line| {
            line.decisions
                .iter()
                .filter(|reply| potential.contains(reply.position.as_str()))
                .scan(None, |earliest, reply| {
                    let position = reply.position.as_str();
                    (*earliest.get_or_insert(position) == position).then_some(reply)
                })
        });
        let reachable: BTreeSet<String> = disagreements(first_reached)
            .into_iter()
            .map(str::to_owned)
            .collect();
        ensure!(!reachable.is_empty(), PreviewUnavailable::CyclicReplies);
        for line in continuations.iter_mut() {
            line.stop_before(|reply| reachable.contains(reply.position.as_str()));
        }
    }
}

fn disagreements<'a>(replies: impl Iterator<Item = &'a ContinuationReply>) -> BTreeSet<&'a str> {
    let mut first = BTreeMap::new();
    let mut competing = BTreeSet::new();
    for reply in replies {
        let position = reply.position.as_str();
        let movement = reply.movement.as_str();
        if *first.entry(position).or_insert(movement) != movement {
            competing.insert(position);
        }
    }
    competing
}

fn authored(input: &catalog::OpeningInput) -> Result<Option<catalog::OpeningPlan>> {
    if input.start_at >= input.route.moves.len() {
        return Ok(None);
    }
    let board = chess::board_at(&input.route, input.start_at)?;
    let remaining = input.route.moves.len() - input.start_at;
    if remaining == 1 && chess::color_name(board.turn()) != input.color {
        return Ok(None);
    }
    Ok(Some(catalog::prepare_opening(input)?))
}

fn format_route(route: &chess::Route) -> Result<String> {
    let Some((reply, prefix)) = route.moves.split_last() else {
        return Ok("Starting position".into());
    };
    let position = chess::position_id(&chess::board_at(route, prefix.len())?);
    gap_pgn::line_text(
        &chess::Route {
            fen: route.fen.clone(),
            moves: prefix.to_vec(),
        },
        &position,
        reply,
    )
}

pub fn plan(
    db: &Connection,
    repertoire: &str,
    color: &str,
    selected: &Choices,
    incoming: &[catalog::OpeningPlan],
) -> Result<Replacement> {
    let proposed = incoming
        .iter()
        .flat_map(|plan| &plan.cards)
        .map(|card| {
            let answer: Vec<String> = serde_json::from_str(&card.answers)?;
            Ok((
                card.position.clone(),
                answer.into_iter().next().context("Missing opening reply")?,
            ))
        })
        .collect::<Result<Choices>>()?;
    ensure!(
        selected
            .iter()
            .all(|(position, movement)| proposed.get(position) == Some(movement)),
        "The selected replacement is no longer in the PGN."
    );
    plan_changes(db, repertoire, color, selected, incoming, &proposed, None)
}

pub fn plan_study_reply(
    db: &Connection,
    repertoire: &str,
    color: &str,
    position: &str,
    current: &str,
    replacement: Option<&str>,
) -> Result<Replacement> {
    let selected = replacement
        .map(|movement| Choices::from([(position.into(), movement.into())]))
        .unwrap_or_default();
    let removal = replacement.is_none().then_some((position, current));
    plan_changes(
        db,
        repertoire,
        color,
        &selected,
        &[],
        &Choices::new(),
        removal,
    )
}

fn plan_changes(
    db: &Connection,
    repertoire: &str,
    color: &str,
    selected: &Choices,
    incoming: &[catalog::OpeningPlan],
    proposed: &Choices,
    removal: Option<(&str, &str)>,
) -> Result<Replacement> {
    let lines = saved_lines(db, repertoire, color)?;
    let mut continuations = lines
        .iter()
        .map(|line| replay(&line.input, selected, removal))
        .collect::<Result<Vec<_>>>()?;

    let addressed = continuations
        .iter()
        .flat_map(|line| &line.decisions)
        .map(|reply| reply.position.as_str())
        .collect::<BTreeSet<_>>();
    let mut current = BTreeMap::new();
    for position in addressed {
        if let Some(reply) = catalog::current_opening_reply(db, repertoire, position)? {
            let version = if reply.quarantined {
                SavedVersion::Quarantined
            } else {
                SavedVersion::Active(reply.version)
            };
            current.insert(position.into(), version);
        }
    }

    for line in &mut continuations {
        stop_at_known_conflict(line, selected, proposed, &current);
    }
    stop_at_reachable_conflicts(&mut continuations, |reply| {
        !proposed.contains_key(&reply.position) && !current.contains_key(&reply.position)
    })?;
    let mut impact = Vec::new();
    let mut archived = Vec::new();
    let mut rewritten = Vec::new();
    let mut removed = BTreeSet::new();
    let mut supported = BTreeSet::new();
    for (line, change) in lines.into_iter().zip(continuations) {
        let previous = catalog::prepare_opening(&line.input)?;
        if change.moves == line.input.route.moves {
            supported.extend(
                previous
                    .cards
                    .into_iter()
                    .map(|card| (card.position, card.version)),
            );
            continue;
        }
        let before = format_route(&line.input.route)?;
        removed.extend(
            previous
                .cards
                .into_iter()
                .map(|card| (card.position, card.version)),
        );
        archived.push(line.id);
        let mut input = line.input;
        input.route.moves = change.moves;
        let next = authored(&input)?;
        impact.push(ChangedLine {
            name: input.name.clone(),
            before,
            after: next
                .as_ref()
                .map(|_| format_route(&input.route))
                .transpose()?,
            stopped_at: change.stopped_at,
        });
        if let Some(next) = next {
            rewritten.push(next);
        }
    }
    let mut installed = BTreeSet::new();
    for plan in rewritten.iter().chain(incoming) {
        installed.extend(
            plan.cards
                .iter()
                .map(|card| (card.position.clone(), card.version.clone())),
        );
    }
    supported.extend(installed.iter().cloned());
    // Current card rows with no surviving authored occurrence are retired, but
    // only if an affected line previously owned that reply.
    let mut retired = BTreeSet::new();
    let active = |position: &str| -> Result<Option<catalog::CurrentOpeningReply>> {
        Ok(catalog::current_opening_reply(db, repertoire, position)?
            .filter(|reply| !reply.quarantined))
    };
    for (position, version) in removed.difference(&supported) {
        if let Some(current) = active(position)?
            && &current.version == version
        {
            retired.insert(current.id);
        }
    }
    for (position, movement) in selected {
        let id = active(position)?
            .context("The saved reply is no longer available.")?
            .id;
        ensure!(
            installed.contains(&(position.clone(), chess::id(&["answer-v1", movement]))),
            "The replacement has no surviving authored route."
        );
        retired.insert(id);
    }
    if let Some((position, _)) = removal {
        let reply = active(position)?.context("The saved reply is no longer available.")?;
        ensure!(
            retired.contains(&reply.id),
            "The removal still has an authored route."
        );
    }
    // Retirement writes the card, its availability and revision projections,
    // up to two live due generations, and up to one committed slot and release.
    const RETIRE_WRITES: usize = 9;
    let mut writes = 1
        + catalog::OPENING_LINE_WRITES * (archived.len() + rewritten.len() + incoming.len())
        + RETIRE_WRITES * retired.len();
    for (position, version) in installed {
        if !active(&position)?.is_some_and(|current| current.version == version) {
            writes += catalog::OPENING_REPLY_WRITES;
        }
    }
    let retired = retired.into_iter().collect::<Vec<_>>();
    Ok(Replacement {
        line_revision: revision(db, repertoire)?,
        impact,
        over_budget: !fits_budget(db, writes, &retired)?,
        base_writes: writes,
        archived,
        rewritten,
        retired,
    })
}

fn fits_budget(db: &Connection, mut writes: usize, retired: &[String]) -> Result<bool> {
    let mut repeats = db.prepare_cached(
        "SELECT count(*) FROM (SELECT 1 FROM repeat_obligation WHERE card_id=?1 LIMIT 43)",
    )?;
    for card in retired {
        if writes > catalog::ONLINE_WRITE_LIMIT {
            return Ok(false);
        }
        writes += 3 * repeats.query_row([card], |row| row.get::<_, usize>(0))?;
    }
    Ok(writes <= catalog::ONLINE_WRITE_LIMIT)
}

#[derive(Debug)]
pub struct OverBudget;

impl std::fmt::Display for OverBudget {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str("This replacement exceeds the atomic save limit. Split the continuation and preview again.")
    }
}
impl std::error::Error for OverBudget {}

pub fn apply(db: &Connection, repertoire: &str, replacement: &Replacement) -> Result<Vec<String>> {
    ensure!(
        fits_budget(db, replacement.base_writes, &replacement.retired)?,
        OverBudget
    );
    ensure!(
        revision(db, repertoire)? == replacement.line_revision,
        crate::gap_edit::Stale
    );
    for id in &replacement.archived {
        db.execute("UPDATE openings SET archived=1 WHERE id=?1", [id])?;
    }
    for id in &replacement.retired {
        db.execute("UPDATE cards SET opening_retired=1 WHERE id=?1", [id])?;
        db.execute("DELETE FROM deck WHERE card_id=?1", [id])?;
        db.execute("DELETE FROM repeat_obligation WHERE card_id=?1", [id])?;
    }
    replacement
        .rewritten
        .iter()
        .map(|line| catalog::apply_opening(db, line, Some(repertoire), true))
        .collect()
}

// Opening edits share the predecessor receipt store. Each command family owns
// its versioned identity and replay format.
pub fn receipt(db: &Connection, command: &str) -> Result<Option<Value>> {
    let saved: Option<String> = db
        .query_row(
            "SELECT reply FROM gap_save_receipt WHERE command=?1",
            [command],
            |row| row.get(0),
        )
        .optional()?;
    Ok(saved.map(|body| serde_json::from_str(&body)).transpose()?)
}

pub fn record_receipt(db: &Connection, command: &str, reply: &Value) -> Result<()> {
    db.execute(
        "INSERT INTO gap_save_receipt VALUES(?1,?2)",
        (command, reply.to_string()),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_discarded_suffix_cannot_trim_an_independent_continuation() -> Result<()> {
        let line = |steps: &[(&str, &str)]| Rewritten {
            moves: steps
                .iter()
                .map(|(_, movement)| (*movement).into())
                .collect(),
            decisions: steps
                .iter()
                .enumerate()
                .map(|(ply, (position, movement))| ContinuationReply {
                    ply,
                    position: (*position).into(),
                    movement: (*movement).into(),
                    label: (*movement).into(),
                })
                .collect(),
            stopped_at: None,
        };
        let mut branches = [
            line(&[("first", "a2a3"), ("later", "e2e4")]),
            line(&[("first", "a2a4")]),
            line(&[("later", "d2d4")]),
        ];
        stop_at_reachable_conflicts(&mut branches, |_| true)?;
        assert!(branches[0].moves.is_empty());
        assert!(branches[1].moves.is_empty());
        assert_eq!(branches[2].moves, ["d2d4"]);
        assert!(branches[2].stopped_at.is_none());
        let mut cyclic = [
            line(&[("first", "a2a3"), ("later", "e2e4")]),
            line(&[("later", "d2d4"), ("first", "a2a4")]),
        ];
        use axum::response::IntoResponse;
        let error = stop_at_reachable_conflicts(&mut cyclic, |_| true).unwrap_err();
        assert_eq!(
            error.downcast_ref::<PreviewUnavailable>(),
            Some(&PreviewUnavailable::CyclicReplies)
        );
        let response = crate::ApiError(error).into_response();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::UNPROCESSABLE_ENTITY
        );
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 4096).await?)?;
        assert_eq!(body["code"], "replacement_preview_unavailable");
        assert_eq!(body["reason"], "cyclic_replies");
        Ok(())
    }

    #[test]
    fn preserves_a_castle_when_its_uci_spelling_differs() -> Result<()> {
        let mut route =
            chess::parse_gap_pgn("1. e4 e5 2. Nf3 Nc6 3. Nc3 Nf6 4. Bb5 a6 5. O-O *")?.main;
        route.moves[8] = "e1h1".into();
        let position = chess::position_id(&chess::board_at(&route, 6)?);
        let selected = Choices::from([(position, "f1c4".into())]);
        let redirected =
            chess::parse_gap_pgn("1. e4 e5 2. Nf3 Nc6 3. Nc3 Nf6 4. Bc4 a6 5. O-O *")?.main;
        let castle = chess::position_id(&chess::board_at(&redirected, 8)?);
        let proposed = Choices::from([(castle, "e1g1".into())]);
        let mut rewrite = replay(
            &catalog::OpeningInput {
                name: "Fixture".into(),
                color: "white".into(),
                route,
                start_at: 0,
            },
            &selected,
            None,
        )?;
        stop_at_known_conflict(&mut rewrite, &selected, &proposed, &BTreeMap::new());
        assert!(rewrite.stopped_at.is_none());
        assert_eq!(rewrite.moves[8], "e1h1");
        Ok(())
    }
}
