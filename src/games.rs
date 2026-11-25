use crate::{
    chess::{self, Pgn, Route},
    game_ir::Mainline,
};
use anyhow::{Context, Result, ensure};
use chrono::{NaiveDate, NaiveTime};
use pgn_reader::{
    Outcome,
    shakmaty::{Color, KnownOutcome, Position},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// An addressed archived game. The chess payload is identical for every importer.
pub struct Game {
    pub id: String,
    pub mainline: Mainline,
    pub learner: Color,
    players: [String; 2],
    occurrence: Occurrence,
    pub outcome: Outcome,
}

/// Source links and display facts are retained alongside the authoritative game.
pub struct Candidate {
    pub game: Game,
    source_url: Option<String>,
    played_on: Option<String>,
}

#[derive(Serialize)]
pub struct Replay {
    id: String,
    white: String,
    black: String,
    learner_color: &'static str,
    route: Route,
    source_url: Option<String>,
    played_on: Option<String>,
}

impl Candidate {
    fn replay(self) -> Replay {
        let [white, black] = self.game.players;
        Replay {
            id: self.game.id,
            white,
            black,
            learner_color: chess::color_name(self.game.learner),
            route: self.game.mainline.route(),
            source_url: self.source_url,
            played_on: self.played_on,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Occurrence([Option<String>; 4]);

impl Occurrence {
    const SHAPES: u8 = 16;

    fn from_pgn(pgn: &Pgn) -> Self {
        let date = |tag: &str| {
            NaiveDate::parse_from_str(pgn.tags.get(tag)?.trim(), "%Y.%m.%d")
                .ok()
                .map(|day| day.to_string())
        };
        let utc = date("UTCDate");
        let utc_time = utc.as_ref().and_then(|_| {
            NaiveTime::parse_from_str(pgn.tags.get("UTCTime")?.trim(), "%H:%M:%S")
                .ok()
                .map(|time| time.format("%H:%M:%S").to_string())
        });
        let round = pgn.tags.get("Round").and_then(|value| {
            let name = value.split_whitespace().collect::<Vec<_>>().join(" ");
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "" | "?" | "-" | "*" | "n/a" | "unknown"
            ) {
                return None;
            }
            if name
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|c| c.is_ascii_digit()))
            {
                return Some(
                    name.split('.')
                        .map(|part| {
                            let number = part.trim_start_matches('0');
                            if number.is_empty() { "0" } else { number }
                        })
                        .collect::<Vec<_>>()
                        .join("."),
                );
            }
            Some(name.to_ascii_lowercase())
        });
        Self([utc, utc_time, date("Date"), round])
    }

    fn shape(&self) -> u8 {
        self.0.iter().enumerate().fold(0, |shape, (i, fact)| {
            shape | (u8::from(fact.is_some()) << i)
        })
    }

    fn key(&self, projection: u8) -> Result<String> {
        Ok(serde_json::to_string(&std::array::from_fn::<_, 4, _>(
            |index| {
                if projection & (1 << index) == 0 {
                    None
                } else {
                    self.0[index].as_deref()
                }
            },
        ))?)
    }

    fn merge(&self, extra: &Self) -> Self {
        Self(std::array::from_fn(|i| {
            self.0[i].as_ref().or(extra.0[i].as_ref()).cloned()
        }))
    }
}

impl Game {
    fn fingerprint(&self, payload: &[u8]) -> String {
        // The mainline encoding is versioned, but archive equivalence uses the
        // V1 canonical bytes, independent of which provider supplied the game.
        let white = self.players[0].trim().to_ascii_lowercase();
        let black = self.players[1].trim().to_ascii_lowercase();
        let moves = blake3::hash(payload).to_hex();
        chess::id(&[
            "game-fingerprint-v1",
            &white,
            &black,
            chess::color_name(self.learner),
            moves.as_str(),
        ])
    }
}

pub fn archive_game(pgn: Pgn, username: &str) -> Result<Candidate> {
    ensure!(pgn.finished, "the game is unfinished");
    let white = pgn.tags.get("White").cloned().unwrap_or_default();
    let black = pgn.tags.get("Black").cloned().unwrap_or_default();
    let plays_white = white.trim().eq_ignore_ascii_case(username);
    let plays_black = black.trim().eq_ignore_ascii_case(username);
    ensure!(
        plays_white != plays_black,
        "the learner must match exactly one player"
    );
    let header = pgn
        .tags
        .get("Result")
        .and_then(|value| value.parse::<KnownOutcome>().ok());
    let terminal = pgn.terminal_outcome.and_then(Outcome::known);
    ensure!(
        terminal.zip(header).is_none_or(|(a, b)| a == b),
        "conflicting game results"
    );
    let outcome = terminal.or(header).context("the game is unfinished")?;
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
    Ok(Candidate {
        game: Game {
            id: format!("{:032x}", rand::random::<u128>()),
            mainline: Mainline::validate(&pgn.main)?,
            learner: if plays_white {
                Color::White
            } else {
                Color::Black
            },
            players: [white, black],
            occurrence: Occurrence::from_pgn(&pgn),
            outcome: Outcome::Known(outcome),
        },
        source_url,
        played_on,
    })
}

fn color(value: &str) -> Result<Color> {
    match value {
        "white" => Ok(Color::White),
        "black" => Ok(Color::Black),
        _ => anyhow::bail!("invalid archived learner side"),
    }
}

type StoredGame = (String, String, String, String, Vec<u8>, String, String);
const FIELDS: &str = "id,white,black,learner_color,game_ir,occurrence,outcome";

fn stored(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredGame> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn decode(row: StoredGame) -> Result<Game> {
    let (id, white, black, learner, ir, occurrence, outcome) = row;
    Ok(Game {
        id,
        mainline: Mainline::decode(&ir)?,
        learner: color(&learner)?,
        players: [white, black],
        occurrence: serde_json::from_str(&occurrence)?,
        outcome: outcome.parse()?,
    })
}

const DISPLAY_FIELDS: &str = "CASE WHEN length(CAST(source_url AS BLOB))<=2048 THEN source_url END,
    substr(played_on,1,64)";
const BROWSE_FIELDS: &str =
    "id,substr(white,1,256),substr(black,1,256),learner_color,game_ir,occurrence,outcome";

pub fn archive(db: &Connection, after: &str, limit: usize) -> Result<Vec<Replay>> {
    let mut query = db.prepare(&format!(
        "SELECT {BROWSE_FIELDS},{DISPLAY_FIELDS} FROM games WHERE id>?1 ORDER BY id LIMIT ?2",
    ))?;
    let mut rows = query.query(params![after, limit])?;
    let mut replays = Vec::new();
    while let Some(row) = rows.next()? {
        replays.push(
            Candidate {
                game: decode(stored(row)?)?,
                source_url: row.get(7)?,
                played_on: row.get(8)?,
            }
            .replay(),
        );
    }
    Ok(replays)
}

pub fn read(db: &Connection, id: &str) -> Result<Game> {
    read_fields(db, id, FIELDS)
}

fn read_fields(db: &Connection, id: &str, fields: &str) -> Result<Game> {
    let row = db
        .query_row(
            &format!("SELECT {fields} FROM games WHERE id=?1"),
            [id],
            stored,
        )
        .optional()?
        .context("game not found")?;
    decode(row)
}

pub fn replay(db: &Connection, id: &str) -> Result<Replay> {
    let snapshot = db.unchecked_transaction()?;
    let game = read_fields(&snapshot, id, BROWSE_FIELDS)?;
    let (source_url, played_on) = snapshot.query_row(
        &format!("SELECT {DISPLAY_FIELDS} FROM games WHERE id=?1"),
        [id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    snapshot.commit()?;
    Ok(Candidate {
        game,
        source_url,
        played_on,
    }
    .replay())
}

pub enum Acceptance {
    Added(String),
    Existing(String),
    ConflictingOutcome,
}

fn compatible(
    db: &Connection,
    fingerprint: &str,
    occurrence: &Occurrence,
) -> Result<Option<(i64, String)>> {
    let incoming = occurrence.shape();
    let mut query = db.prepare_cached(
        "SELECT ordinal,game_id FROM game_occurrence
         WHERE fingerprint=?1 AND known=?2 AND projection=?3 AND facts=?4
         ORDER BY ordinal LIMIT 1",
    )?;
    let mut best: Option<(u32, i64, String)> = None;
    for known in 0..Occurrence::SHAPES {
        let shared = incoming & known;
        let score = shared.count_ones();
        if best.as_ref().is_some_and(|(prior, _, _)| *prior > score) {
            continue;
        }
        if let Some((ordinal, id)) = query
            .query_row(
                params![fingerprint, known, shared, occurrence.key(shared)?],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
            && best
                .as_ref()
                .is_none_or(|(prior, previous_id, _)| score > *prior || ordinal < *previous_id)
        {
            best = Some((score, ordinal, id));
        }
    }
    Ok(best.map(|(_, ordinal, id)| (ordinal, id)))
}

fn index_game(
    db: &Connection,
    id: &str,
    ordinal: i64,
    fingerprint: &str,
    occurrence: &Occurrence,
) -> Result<()> {
    let shape = occurrence.shape();
    let mut insert = db.prepare_cached("INSERT INTO game_occurrence VALUES (?1,?2,?3,?4,?5,?6)")?;
    let mut projection = shape;
    loop {
        insert.execute(params![
            fingerprint,
            shape,
            projection,
            occurrence.key(projection)?,
            ordinal,
            id
        ])?;
        if projection == 0 {
            break;
        }
        projection = (projection - 1) & shape;
    }
    Ok(())
}

pub fn accept_game(db: &Connection, candidate: &Candidate) -> Result<Acceptance> {
    let game = &candidate.game;
    let payload = game.mainline.encode();
    let fingerprint = game.fingerprint(&payload);
    if let Some((ordinal, id)) = compatible(db, &fingerprint, &game.occurrence)? {
        let (facts, result): (String, String) = db.query_row(
            "SELECT occurrence,outcome FROM games WHERE id=?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let outcome: Outcome = result.parse()?;
        if outcome.is_known() && game.outcome.is_known() && outcome != game.outcome {
            return Ok(Acceptance::ConflictingOutcome);
        }
        let facts: Occurrence = serde_json::from_str(&facts)?;
        let merged = facts.merge(&game.occurrence);
        if merged != facts {
            db.execute("DELETE FROM game_occurrence WHERE game_id=?1", [&id])?;
            index_game(db, &id, ordinal, &fingerprint, &merged)?;
        }
        db.execute(
            "UPDATE games SET occurrence=?2,outcome=?3,source_url=coalesce(source_url,?4),
              played_on=coalesce(played_on,?5) WHERE id=?1 AND
              (occurrence<>?2 OR outcome<>?3 OR (source_url IS NULL AND ?4 IS NOT NULL)
              OR (played_on IS NULL AND ?5 IS NOT NULL))",
            params![
                id,
                serde_json::to_string(&merged)?,
                outcome
                    .known()
                    .or(game.outcome.known())
                    .map_or("*", KnownOutcome::as_str),
                candidate.source_url,
                candidate.played_on
            ],
        )?;
        return Ok(Acceptance::Existing(id));
    }
    db.execute(
        "INSERT INTO games(id,white,black,learner_color,game_ir,source_url,played_on,occurrence,outcome)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![game.id, game.players[0], game.players[1], chess::color_name(game.learner), payload,
            candidate.source_url, candidate.played_on, serde_json::to_string(&game.occurrence)?, game.outcome.to_string()],
    )?;
    index_game(
        db,
        &game.id,
        db.last_insert_rowid(),
        &fingerprint,
        &game.occurrence,
    )?;
    Ok(Acceptance::Added(game.id.clone()))
}

/// The recorded predecessor is disclosed only when the replay reaches the issued position.
pub fn preceding_move(
    db: &Connection,
    id: &str,
    ply: usize,
    issued: &str,
) -> Result<Option<String>> {
    let game = read(db, id)?;
    if ply == 0 || ply > game.mainline.plies() {
        return Ok(None);
    }
    let mut board = chess::position(game.mainline.start())?;
    let mut last = None;
    for notation in game.mainline.moves().take(ply) {
        let played = chess::parse_move(&board, &notation)?;
        board.play_unchecked(played);
        last = Some(notation);
    }
    let issued = chess::position(issued)?;
    Ok(last.filter(|_| chess::position_id(&board) == chess::position_id(&issued)))
}

pub fn mismatch_context(
    db: &Connection,
    position_id: &str,
    repertoire: &str,
    version: &str,
) -> Result<Option<(String, usize, String)>> {
    Ok(db.query_row("SELECT game_id,ply,played_move FROM mismatches WHERE position_id=?1 AND repertoire_id=?2 AND answer_version=?3 LIMIT 1",
        params![position_id,repertoire,version], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?)
}

/// Used only by predecessor migrations before the archive table is converted.
pub(crate) fn legacy_route(db: &Connection, id: &str) -> Result<Route> {
    let route: String = db.query_row("SELECT route FROM games WHERE id=?1", [id], |r| r.get(0))?;
    Ok(serde_json::from_str(&route)?)
}

pub(crate) fn upgrade(db: &Connection) -> Result<()> {
    db.execute_batch(include_str!("game_ir.sql"))?;
    {
        let mut before = db.prepare(
            "SELECT rowid,id,white,black,learner_color,route,source_url,played_on FROM games ORDER BY rowid",
        )?;
        let mut rows = before.query([])?;
        let mut insert =
            db.prepare("INSERT INTO games_v7 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'*')")?;
        while let Some(row) = rows.next()? {
            let route: Route = serde_json::from_str(row.get_ref(5)?.as_str()?)?;
            let mainline = Mainline::validate(&route)?;
            let payload = mainline.encode();
            let decoded = Mainline::decode(&payload)?.route();
            ensure!(
                decoded.fen == route.fen && decoded.moves == route.moves,
                "retained archive changed during conversion"
            );
            insert.execute(params![
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                payload,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                serde_json::to_string(&Occurrence::default())?
            ])?;
        }
    }
    db.execute_batch("DROP TABLE games; ALTER TABLE games_v7 RENAME TO games;")?;
    db.execute_batch(
        "CREATE TABLE game_occurrence (
            fingerprint TEXT NOT NULL, known INTEGER NOT NULL, projection INTEGER NOT NULL,
            facts TEXT NOT NULL, ordinal INTEGER NOT NULL, game_id TEXT NOT NULL REFERENCES games(id),
            PRIMARY KEY(fingerprint,known,projection,facts,ordinal)
         ) WITHOUT ROWID, STRICT;
         CREATE INDEX game_occurrence_id ON game_occurrence(game_id);",
    )?;
    let mut scan = db.prepare(&format!("SELECT {FIELDS},rowid FROM games ORDER BY rowid"))?;
    let mut rows = scan.query([])?;
    while let Some(row) = rows.next()? {
        let ordinal: i64 = row.get(7)?;
        let game = decode(stored(row)?)?;
        let fingerprint = game.fingerprint(&game.mainline.encode());
        index_game(db, &game.id, ordinal, &fingerprint, &game.occurrence)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn archive_store() -> Result<Connection> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(include_str!("schema.sql"))?;
        upgrade(&db)?;
        Ok(db)
    }

    fn intake(extra: &str, result: &str) -> Result<Candidate> {
        let pgn = format!("[White \"Learner\"]\n[Black \"Opponent\"]\n{extra}\n1. e4 e5 {result}");
        archive_game(chess::parse_pgn(&pgn, false, 1)?.remove(0)?, "Learner")
    }

    fn id(result: Acceptance) -> String {
        match result {
            Acceptance::Added(id) | Acceptance::Existing(id) => id,
            Acceptance::ConflictingOutcome => panic!("unexpected outcome conflict"),
        }
    }

    // Independent read-only use of the addressed representation.
    fn summarize(games: impl IntoIterator<Item = Game>) -> (usize, usize) {
        games.into_iter().fold((0, 0), |(plies, wins), game| {
            (
                plies + game.mainline.plies(),
                wins + usize::from(game.outcome.winner() == Some(game.learner)),
            )
        })
    }

    #[test]
    fn source_metadata_and_result_have_separate_roles() -> Result<()> {
        let mut db = archive_store()?;
        let game = intake("[Date \"2026.05.15\"]\n", "1-0")?;
        let other_basis = intake(
            "[UTCDate \"2026.05.16\"]\n[UTCTime \"00:00:15\"]\n[Site \"https://lichess.org/game\"]\n",
            "1-0",
        )?;
        let distinct = intake(
            "[UTCDate \"2026.05.16\"]\n[UTCTime \"00:02:30\"]\n",
            "1/2-1/2",
        )?;
        let disagreement = intake(
            "[UTCDate \"2026.05.16\"]\n[UTCTime \"00:00:15\"]\n",
            "1/2-1/2",
        )?;
        let tx = db.transaction()?;
        let first = id(accept_game(&tx, &game)?);
        assert!(matches!(accept_game(&tx, &other_basis)?, Acceptance::Existing(id) if id == first));
        let second = id(accept_game(&tx, &distinct)?);
        assert_ne!(first, second);
        assert!(matches!(
            accept_game(&tx, &disagreement)?,
            Acceptance::ConflictingOutcome
        ));
        let sparse = intake("[Link \"https://www.chess.com/game\"]\n", "1-0")?;
        assert!(matches!(accept_game(&tx, &sparse)?, Acceptance::Existing(id) if id == first));
        tx.commit()?;
        assert_eq!(summarize([read(&db, &first)?, read(&db, &second)?]), (4, 1));
        assert!(intake("", "*").is_err());
        assert!(intake("[Result \"1-0\"]\n", "0-1").is_err());
        Ok(())
    }

    #[test]
    fn named_rounds_distinguish_otherwise_identical_games() -> Result<()> {
        let db = archive_store()?;
        let semifinal = intake("[Round \" Semifinal \"]\n", "1-0")?;
        let spelling = intake("[Round \"SEMIFINAL\"]\n", "1-0")?;
        let final_round = intake("[Round \"Final\"]\n", "1-0")?;
        let unknown = intake("[Round \"?\"]\n", "1-0")?;
        let first = id(accept_game(&db, &semifinal)?);
        assert_eq!(id(accept_game(&db, &spelling)?), first);
        assert_ne!(id(accept_game(&db, &final_round)?), first);
        assert_eq!(id(accept_game(&db, &unknown)?), first);
        Ok(())
    }

    #[test]
    fn occurrence_lookup_work_is_bounded_with_alias_collisions() -> Result<()> {
        let db = archive_store()?;
        let candidate = intake("[Round \"2\"]\n", "1-0")?;
        let game = &candidate.game;
        let fingerprint = game.fingerprint(&game.mainline.encode());

        fn steps(db: &Connection, fingerprint: &str, occurrence: &Occurrence) -> Result<usize> {
            let counter = Arc::new(AtomicUsize::new(0));
            let count = counter.clone();
            db.progress_handler(
                1,
                Some(move || {
                    count.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            );
            let selection = compatible(db, fingerprint, occurrence);
            db.progress_handler(0, None::<fn() -> bool>);
            assert!(selection?.is_none());
            Ok(counter.load(Ordering::Relaxed))
        }

        let before = steps(&db, &fingerprint, &game.occurrence)?;
        let contradictory = Occurrence([None, None, None, Some("1".into())]);
        db.execute(
            "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<100000)
             INSERT INTO games(id,white,black,learner_color,game_ir,occurrence,outcome)
             SELECT prefix||i,'Learner','Opponent','white',?1,?2,'1-0'
             FROM n CROSS JOIN (SELECT 'unmatched-' AS prefix UNION ALL SELECT 'unrelated-')",
            params![
                game.mainline.encode(),
                serde_json::to_string(&contradictory)?
            ],
        )?;
        db.execute(
            "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<100000)
             INSERT INTO game_occurrence
             SELECT ?1,8,8,?2,i,printf('unmatched-%d',i) FROM n",
            params![fingerprint, contradictory.key(8)?],
        )?;
        db.execute(
            "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<100000)
             INSERT INTO game_occurrence
             SELECT printf('unrelated-%d',i),8,8,?1,i,printf('unrelated-%d',i) FROM n",
            [game.occurrence.key(8)?],
        )?;
        let after = steps(&db, &fingerprint, &game.occurrence)?;
        assert!(
            after <= before + 25,
            "lookup grew from {before} to {after} steps"
        );
        Ok(())
    }
}
