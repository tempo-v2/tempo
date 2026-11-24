//! Offline adapter for the legacy repertoire schema.
use crate::{
    catalog::{self, OpeningInput},
    chess::{self, Route},
    db::Database,
};
use anyhow::{Context, Result, ensure};
use pgn_reader::shakmaty::Position;
use rusqlite::{Connection, OpenFlags, params};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Deserialize)]
struct Item {
    label: String,
    definition: Definition,
}

#[derive(Deserialize)]
struct Definition {
    decisions: Vec<Decision>,
}

#[derive(Deserialize)]
struct Decision {
    fen: String,
    expected: Expected,
}

#[derive(Deserialize)]
struct Expected {
    uci: String,
}

type Answers = BTreeSet<(String, String)>;

fn answers(route: &Route, color: &str) -> Result<Answers> {
    let mut board = chess::position(&route.fen)?;
    let mut answers = Answers::new();
    for input in &route.moves {
        let m = chess::parse_move(&board, input)?;
        if chess::color_name(board.turn()) == color {
            answers.insert((chess::position_id(&board), chess::uci(m)));
        }
        board.play_unchecked(m);
    }
    Ok(answers)
}

fn decode(json: &str) -> Result<(String, String, String)> {
    let item: Item = serde_json::from_str(json)?;
    let decision = item
        .definition
        .decisions
        .first()
        .context("no stored repertoire decision")?;
    let board = chess::position(&decision.fen)?;
    let expected = chess::uci(chess::parse_move(&board, &decision.expected.uci)?);
    Ok((item.label, chess::fen(&board), expected))
}

fn reconstruct(db: &Connection, dataset: &str, book: &str) -> Result<Vec<Route>> {
    let mut q = db.prepare(
        "SELECT line.id,current.item FROM repertoire_lines line
        JOIN line_targets member ON member.line_id=line.id
        JOIN current_content current ON current.target_id=member.target_id
        WHERE line.repertoire=?1 AND current.dataset_id=?2 ORDER BY line.ordinal,member.ordinal",
    )?;
    let rows = q.query_map(params![book, dataset], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut lines = BTreeMap::<String, Vec<(String, String)>>::new();
    for row in rows {
        let (id, json) = row?;
        let (_, fen, m) = decode(&json)?;
        lines.entry(id).or_default().push((fen, m));
    }
    let mut routes = vec![];
    for (id, decisions) in lines {
        let mut board = chess::position(&decisions[0].0)?;
        let mut route = Route {
            fen: chess::fen(&board),
            moves: vec![],
        };
        for (index, (expected_fen, m)) in decisions.into_iter().enumerate() {
            let target = chess::position_id(&chess::position(&expected_fen)?);
            if index > 0 {
                let candidates = board
                    .legal_moves()
                    .into_iter()
                    .filter(|&m| {
                        let mut reached = board.clone();
                        reached.play_unchecked(m);
                        chess::position_id(&reached) == target
                    })
                    .collect::<Vec<_>>();
                ensure!(
                    candidates.len() == 1,
                    "cannot prove the opponent reply in stored line {id}"
                );
                board.play_unchecked(candidates[0]);
                route.moves.push(chess::uci(candidates[0]));
            }
            ensure!(
                chess::position_id(&board) == target,
                "the stored route does not reach its target"
            );
            board.play_unchecked(chess::parse_move(&board, &m)?);
            route.moves.push(m);
        }
        routes.push(route);
    }
    ensure!(!routes.is_empty(), "no verifiable stored lines");
    Ok(routes)
}

pub fn import(source: &Path, target: &Path) -> Result<()> {
    ensure!(
        source.canonicalize().ok() != target.canonicalize().ok(),
        "choose a separate database"
    );
    let source = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // Read every source row under one SQLite snapshot, including any WAL state.
    source.execute_batch("PRAGMA query_only=ON; BEGIN")?;
    let dataset: String = source.query_row(
        "SELECT dataset_id FROM store_state WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    let mut q = source.prepare(
        "SELECT repertoire,item FROM current_content
        WHERE dataset_id=?1 AND repertoire IS NOT NULL ORDER BY repertoire,target_id",
    )?;
    let rows = q.query_map([&dataset], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut books = BTreeMap::<String, Vec<(String, String, String)>>::new();
    for row in rows {
        let (id, json) = row?;
        books.entry(id).or_default().push(decode(&json)?);
    }
    ensure!(
        !books.is_empty(),
        "there are no current opening repertoires in the source"
    );
    let mut destination = Database::init(target)?;
    let tx = destination.transaction()?;
    let mut imported = vec![];
    let mut gaps = vec![];
    for (book, items) in books {
        let (name, fen, _) = &items[0];
        let color = chess::color_name(chess::position(fen)?.turn());
        let expected = items
            .iter()
            .map(|(_, fen, m)| {
                let board = chess::position(fen)?;
                ensure!(
                    chess::color_name(board.turn()) == color,
                    "mixed practice colors"
                );
                Ok((chess::position_id(&board), m.clone()))
            })
            .collect::<Result<Answers>>()?;
        let pgn: Option<String> = source.query_row("SELECT (SELECT s.pgn FROM repertoire_documents d
            JOIN content_sources s ON s.id=d.source_id WHERE d.dataset_id=?1 AND d.repertoire_id=?2)",
            params![dataset,book],|r|r.get(0))?;
        let result = (|| -> Result<usize> {
            let routes = if let Some(pgn) = pgn {
                let games = chess::parse_pgn(&pgn, true, 20_000)?;
                let mut routes = vec![];
                for game in games {
                    let game = game?;
                    routes.push(game.main);
                    routes.extend(game.variations);
                }
                routes
            } else {
                reconstruct(&source, &dataset, &book)?
            };
            let mut actual = Answers::new();
            let mut count = 0;
            for route in routes {
                let found = answers(&route, color)?;
                if found.is_empty() {
                    continue;
                }
                actual.extend(found);
                catalog::save_opening(
                    &tx,
                    &OpeningInput {
                        name: name.clone(),
                        color: color.into(),
                        route,
                        start_at: 0,
                    },
                    Some(&book),
                )?;
                count += 1;
            }
            ensure!(
                expected == actual,
                "opening reconciliation differs: {} stored responses, {} reconstructed responses ({} missing, {} extra)",
                expected.len(),
                actual.len(),
                expected.difference(&actual).count(),
                actual.difference(&expected).count()
            );
            Ok(count)
        })();
        match result {
            Ok(count) => imported.push(format!(
                "{name}: {count} verified routes, {} responses",
                expected.len()
            )),
            Err(error) => gaps.push(format!("{name}: {error:#}")),
        }
    }
    ensure!(
        gaps.is_empty(),
        "opening import rolled back:\n{}",
        gaps.join("\n")
    );
    tx.commit()?;
    for result in imported {
        println!("{result}");
    }
    Ok(())
}
