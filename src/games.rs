use crate::chess::{self, Pgn, Route};
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

pub fn accept_game(db: &Connection, game: &Game) -> Result<bool> {
    Ok(db.execute(
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
    )? == 1)
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
