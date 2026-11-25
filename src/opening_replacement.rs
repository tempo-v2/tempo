use crate::{catalog, chess, gap_pgn};
use anyhow::{Context, Result, ensure};
use pgn_reader::shakmaty::Position;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
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
        "SELECT id,name,route,start_at FROM openings
         WHERE repertoire_id=?1 AND color=?2 AND archived=0 ORDER BY id LIMIT 2001",
    )?;
    let mut rows = query.query((repertoire, color))?;
    let mut lines = Vec::new();
    let mut moves = 0;
    while let Some(row) = rows.next()? {
        let route: chess::Route = serde_json::from_str(&row.get::<_, String>(2)?)?;
        moves += route.moves.len();
        ensure!(
            lines.len() < 2000 && moves <= 100_000,
            "This repertoire is too large to preview a replacement here."
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
    new_replies: Vec<(String, String)>,
    stopped_at: Option<String>,
}

struct Rules<'a> {
    db: &'a Connection,
    repertoire: &'a str,
    proposed: &'a Choices,
    selected: &'a Choices,
    competing: &'a BTreeSet<String>,
}

impl Rules<'_> {
    fn rewrite(&self, input: &catalog::OpeningInput) -> Result<Rewritten> {
        let mut result = Rewritten::default();
        let mut board = chess::position(&input.route.fen)?;
        let mut original_board = board.clone();
        let mut changed = false;
        for (ply, original) in input.route.moves.iter().enumerate() {
            let position = chess::position_id(&board);
            let original_move = chess::parse_move(&board, original).ok();
            let owned = ply >= input.start_at && chess::color_name(board.turn()) == input.color;
            let movement = match self.selected.get(&position).filter(|_| owned) {
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
                let version = chess::id(&["answer-v1", &canonical]);
                let mut lookup = self.db.prepare_cached(
                    "SELECT answer_version,quarantined FROM cards
                     WHERE kind='opening' AND repertoire_id=?1 AND position_id=?2 AND opening_retired=0
                     ORDER BY quarantined LIMIT 1",
                )?;
                let current: Option<(String, bool)> = lookup
                    .query_row((self.repertoire, &position), |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })
                    .optional()?;
                let incompatible = match self
                    .selected
                    .get(&position)
                    .or_else(|| self.proposed.get(&position))
                {
                    Some(expected) => *expected != canonical,
                    None => {
                        current
                            .as_ref()
                            .is_some_and(|(saved, quarantined)| *quarantined || *saved != version)
                            || self.competing.contains(&position)
                    }
                };
                if incompatible {
                    result.stopped_at = Some(format!(
                        "{} would introduce another reply conflict.",
                        gap_pgn::reply_text(&board, &canonical)?
                    ));
                    break;
                }
                if current.is_none() && !self.proposed.contains_key(&position) {
                    result.new_replies.push((position, canonical.clone()));
                }
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
    let lines = saved_lines(db, repertoire, color)?;
    let mut competing = BTreeSet::new();
    let empty = BTreeSet::new();
    let rules = Rules {
        db,
        repertoire,
        proposed: &proposed,
        selected,
        competing: &empty,
    };
    // Never resolve two newly colliding saved continuations by picking a line
    // in database order. Stop both before the disagreement.
    let mut replies = Choices::new();
    for line in &lines {
        for (position, played) in rules.rewrite(&line.input)?.new_replies {
            if replies
                .insert(position.clone(), played.clone())
                .is_some_and(|previous| previous != played)
            {
                competing.insert(position);
            }
        }
    }
    let rules = Rules {
        db,
        repertoire,
        proposed: &proposed,
        selected,
        competing: &competing,
    };
    let mut impact = Vec::new();
    let mut archived = Vec::new();
    let mut rewritten = Vec::new();
    let mut removed = BTreeSet::new();
    let mut supported = BTreeSet::new();
    for line in lines {
        let previous = catalog::prepare_opening(&line.input)?;
        let change = rules.rewrite(&line.input)?;
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
    let mut lookup = db.prepare_cached(
        "SELECT id,answer_version FROM cards
         WHERE kind='opening' AND repertoire_id=?1 AND position_id=?2 AND opening_retired=0 AND quarantined=0",
    )?;
    let active = |position: &str| -> Result<Option<(String, String)>> {
        Ok(lookup
            .query_row((repertoire, position), |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?)
    };
    let mut active = active;
    for (position, version) in removed.difference(&supported) {
        if let Some((id, current)) = active(position)?
            && &current == version
        {
            retired.insert(id);
        }
    }
    for (position, movement) in selected {
        let id = active(position)?
            .context("The saved reply is no longer available.")?
            .0;
        ensure!(
            proposed.get(position) == Some(movement),
            "The selected replacement is no longer in the PGN."
        );
        retired.insert(id);
    }
    let mut writes =
        1 + 3 * (archived.len() + rewritten.len() + incoming.len()) + 5 * retired.len();
    for (position, version) in installed {
        if !active(&position)?.is_some_and(|(_, current)| current == version) {
            writes += 3;
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
        if writes > 128 {
            return Ok(false);
        }
        writes += 3 * repeats.query_row([card], |row| row.get::<_, usize>(0))?;
    }
    Ok(writes <= 128)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_a_castle_when_its_uci_spelling_differs() -> Result<()> {
        let files = tempfile::tempdir()?;
        let db = crate::Database::init(&files.path().join("test.sqlite3"))?;
        let mut route =
            chess::parse_gap_pgn("1. e4 e5 2. Nf3 Nc6 3. Nc3 Nf6 4. Bb5 a6 5. O-O *")?.main;
        route.moves[8] = "e1h1".into();
        let position = chess::position_id(&chess::board_at(&route, 6)?);
        let selected = Choices::from([(position, "f1c4".into())]);
        let redirected =
            chess::parse_gap_pgn("1. e4 e5 2. Nf3 Nc6 3. Nc3 Nf6 4. Bc4 a6 5. O-O *")?.main;
        let castle = chess::position_id(&chess::board_at(&redirected, 8)?);
        let proposed = Choices::from([(castle, "e1g1".into())]);
        let rules = Rules {
            db: &db,
            repertoire: "fixture",
            proposed: &proposed,
            selected: &selected,
            competing: &BTreeSet::new(),
        };
        let rewrite = rules.rewrite(&catalog::OpeningInput {
            name: "Fixture".into(),
            color: "white".into(),
            route,
            start_at: 0,
        })?;
        assert!(rewrite.stopped_at.is_none());
        assert_eq!(rewrite.moves[8], "e1h1");
        Ok(())
    }
}
