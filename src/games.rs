use crate::{
    catalog::{self, TacticInput},
    chess::{self, Pgn, Route},
};
use anyhow::{Result, ensure};
use pgn_reader::shakmaty::Position;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Game {
    pub id: String,
    pub white: String,
    pub black: String,
    pub learner_color: String,
    pub route: Route,
    pub source_url: Option<String>,
    pub played_on: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RepertoireMismatch {
    pub game_id: String,
    pub ply: usize,
    pub position_id: String,
    pub repertoire_id: String,
    pub answer_version: String,
    pub played_move: String,
}

#[derive(Debug)]
struct MissedCaptureFound {
    ply: usize,
    position_id: String,
    fen: String,
    played_move: String,
    captures: Vec<String>,
}

pub fn archive_game(pgn: Pgn, username: &str) -> Result<Game> {
    ensure!(pgn.finished, "the game is unfinished");
    let white = pgn.tags.get("White").cloned().unwrap_or_default();
    let black = pgn.tags.get("Black").cloned().unwrap_or_default();
    let plays_white = white.eq_ignore_ascii_case(username);
    let plays_black = black.eq_ignore_ascii_case(username);
    ensure!(
        plays_white != plays_black,
        "the learner must match exactly one player"
    );
    let color = if plays_white { "white" } else { "black" };
    let source_url = pgn
        .tags
        .get("Link")
        .or_else(|| pgn.tags.get("Site"))
        .filter(|url| {
            url.starts_with("https://lichess.org/")
                || url.starts_with("https://www.chess.com/")
                || url.starts_with("https://chess.com/")
        })
        .cloned();
    let played_on = pgn
        .tags
        .get("UTCDate")
        .or_else(|| pgn.tags.get("Date"))
        .cloned();
    let game_ref = source_url
        .as_deref()
        .map(|url| url.split(['?', '#']).next().unwrap_or(url));
    let id = if let Some(reference) = game_ref {
        chess::id(&[
            "provider-game-v1",
            reference,
            &username.to_ascii_lowercase(),
        ])
    } else {
        chess::id(&[
            "pgn-game-v1",
            &white.to_ascii_lowercase(),
            &black.to_ascii_lowercase(),
            color,
            played_on.as_deref().unwrap_or(""),
            pgn.tags.get("UTCTime").map_or("", String::as_str),
            pgn.tags.get("Round").map_or("", String::as_str),
            &pgn.main.fen,
            &serde_json::to_string(&pgn.main.moves)?,
        ])
    };
    Ok(Game {
        id,
        white,
        black,
        learner_color: color.into(),
        route: pgn.main,
        source_url,
        played_on,
    })
}

type GameRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<GameRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}

pub fn archive(db: &Connection, after: &str, limit: usize) -> Result<Vec<Game>> {
    let mut q = db.prepare("SELECT id,white,black,learner_color,route,source_url,played_on FROM games WHERE id>?1 ORDER BY id LIMIT ?2")?;
    let rows = q.query_map(params![after, limit], row)?;
    rows.map(|r| decode(r?)).collect()
}

fn decode(row: GameRow) -> Result<Game> {
    let (id, white, black, learner_color, route, source_url, played_on) = row;
    Ok(Game {
        id,
        white,
        black,
        learner_color,
        route: serde_json::from_str(&route)?,
        source_url,
        played_on,
    })
}

pub fn read(db: &Connection, id: &str) -> Result<Game> {
    use rusqlite::OptionalExtension;
    let row = db
        .query_row(
            "SELECT id,white,black,learner_color,route,source_url,played_on FROM games WHERE id=?1",
            [id],
            row,
        )
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("game not found"))?;
    decode(row)
}

fn accept_game(db: &mut Connection, game: &Game, job_id: i64) -> Result<bool> {
    use rusqlite::OptionalExtension;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(scanned) = tx
        .query_row(
            "SELECT scanned FROM job_games WHERE job_id=?1 AND game_id=?2",
            params![job_id, game.id],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
    {
        return Ok(scanned);
    }
    let added = tx.execute(
        "INSERT OR IGNORE INTO games VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            game.id,
            game.white,
            game.black,
            game.learner_color,
            serde_json::to_string(&game.route)?,
            game.source_url,
            game.played_on
        ],
    )?;
    tx.execute(
        "INSERT INTO job_games(job_id,game_id) VALUES (?1,?2)",
        params![job_id, game.id],
    )?;
    tx.execute(
        "UPDATE jobs SET added=added+?1,existing=existing+?2 WHERE id=?3",
        params![added, 1 - added, job_id],
    )?;
    tx.commit()?;
    Ok(false)
}

/// Return a recorded predecessor only when the source replay reaches the card.
pub fn preceding_move(
    db: &Connection,
    game_id: &str,
    ply: usize,
    issued: &str,
) -> Result<Option<String>> {
    let game = read(db, game_id)?;
    if ply == 0 || ply > game.route.moves.len() {
        return Ok(None);
    }
    let mut board = chess::board_at(&game.route, ply - 1)?;
    let played = chess::parse_move(&board, &game.route.moves[ply - 1])?;
    board.play_unchecked(played);
    let issued = chess::position(issued)?;
    Ok((chess::position_id(&board) == chess::position_id(&issued)).then(|| chess::uci(played)))
}

pub fn scan_and_store(db: &mut Connection, game: &Game, job_id: i64) -> Result<()> {
    if accept_game(db, game, job_id)? {
        return Ok(());
    }
    // Scan the archived identity/content, even if a provider sends different metadata later.
    let archived = read(db, &game.id)?;
    let game = &archived;

    let mut board = chess::position(&game.route.fen)?;
    let mut mismatches = Vec::new();
    let mut captures = Vec::new();
    for (ply, played) in game.route.moves.iter().enumerate() {
        let m = chess::parse_move(&board, played)?;
        if chess::color_name(board.turn()) == game.learner_color {
            let position_id = chess::position_id(&board);
            let reference = catalog::answers_at(db, &position_id, &game.learner_color)?;
            // A move prescribed by any matching repertoire is valid play.
            if !reference.iter().any(|item| item.moves.contains(played)) {
                mismatches.extend(reference.into_iter().map(|item| RepertoireMismatch {
                    game_id: game.id.clone(),
                    ply,
                    position_id: position_id.clone(),
                    repertoire_id: item.repertoire_id,
                    answer_version: item.answer_version,
                    played_move: played.clone(),
                }));
            }
            let options = chess::missed_captures(&board, m);
            if !options.is_empty() {
                captures.push(MissedCaptureFound {
                    ply,
                    position_id,
                    fen: chess::fen(&board),
                    played_move: played.clone(),
                    captures: options,
                });
            }
        }
        board.play_unchecked(m);
    }

    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for event in mismatches {
        tx.execute(
            "INSERT OR IGNORE INTO mismatches VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                event.game_id,
                event.ply,
                event.position_id,
                event.repertoire_id,
                event.answer_version,
                event.played_move
            ],
        )?;
    }
    let mut found = 0;
    for event in &captures {
        found += tx.execute(
            "INSERT OR IGNORE INTO missed_captures VALUES (?1,?2,?3,?4,?5)",
            params![
                game.id,
                event.ply,
                event.position_id,
                event.played_move,
                serde_json::to_string(&event.captures)?
            ],
        )?;
    }
    tx.execute(
        "UPDATE jobs SET found=found+?1 WHERE id=?2",
        params![found, job_id],
    )?;
    tx.commit()?;

    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for event in captures {
        let name = format!("Missed capture · move {}", (event.ply / 2) + 1);
        catalog::publish_tactic(
            &tx,
            TacticInput {
                name: &name,
                route: &Route {
                    fen: event.fen,
                    moves: vec![event.captures[0].clone()],
                },
                answers: &event.captures,
                source: Some((&game.id, event.ply, &event.played_move)),
            },
        )?;
    }
    tx.execute(
        "UPDATE job_games SET scanned=1 WHERE job_id=?1 AND game_id=?2",
        params![job_id, game.id],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn mismatch_context(
    db: &Connection,
    position_id: &str,
    repertoire: &str,
    version: &str,
) -> Result<Option<(String, usize, String)>> {
    use rusqlite::OptionalExtension;
    Ok(db.query_row("SELECT game_id,ply,played_move FROM mismatches WHERE position_id=?1 AND repertoire_id=?2 AND answer_version=?3 LIMIT 1",
        params![position_id,repertoire,version], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn puzzle_write_failure_keeps_raw_game_and_finding_for_retry() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        db.execute_batch(include_str!("schema.sql"))?;
        db.execute_batch(include_str!("opening_policy.sql"))?;
        db.execute("INSERT INTO jobs(provider,username,operation,status) VALUES ('pgn','Learner','upload','running')", [])?;
        let text = "[White \"Learner\"]\n[Black \"Opponent\"]\n[Result \"1/2-1/2\"]\n[SetUp \"1\"]\n[FEN \"4k3/8/8/3n4/8/8/8/3RK3 w - - 0 1\"]\n\n1. Ke2 1/2-1/2";
        let game = archive_game(chess::parse_pgn(text, false, 1)?.remove(0)?, "Learner")?;
        db.execute_batch("CREATE TRIGGER fail_puzzle BEFORE INSERT ON tactics BEGIN SELECT raise(abort,'fixture failure'); END;")?;
        assert!(scan_and_store(&mut db, &game, 1).is_err());
        assert_eq!(read(&db, &game.id)?.id, game.id);
        assert_eq!(
            db.query_row("SELECT found FROM jobs", [], |r| r.get::<_, i64>(0))?,
            1
        );
        db.execute_batch("DROP TRIGGER fail_puzzle;")?;
        scan_and_store(&mut db, &game, 1)?;
        scan_and_store(&mut db, &game, 1)?;
        assert_eq!(catalog::tactics(&db, "")?.len(), 1);
        assert_eq!(
            db.query_row("SELECT found FROM jobs", [], |r| r.get::<_, i64>(0))?,
            1
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM learning", [], |r| r.get::<_, i64>(0))?,
            0
        );
        Ok(())
    }
}
