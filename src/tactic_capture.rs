//! Explicit Capture acceptance, separate from background publication and Study.
use crate::{catalog, chess, db, tactic_import};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

#[derive(Debug)]
pub enum Rejected {
    Invalid(String),
    Lookup(String),
    Dataset,
    Conflict,
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) | Self::Lookup(message) => f.write_str(message),
            Self::Dataset => {
                f.write_str("This capture belongs to another Tempo library. Reopen Capture.")
            }
            Self::Conflict => {
                f.write_str("This capture was already submitted with different content.")
            }
        }
    }
}

impl std::error::Error for Rejected {}

#[derive(Deserialize, Serialize)]
pub struct Command<T> {
    pub dataset: String,
    pub request: String,
    pub input: T,
}

#[derive(Deserialize, Serialize)]
pub struct LichessInput {
    pub id: String,
    pub name: Option<String>,
}

pub struct Key {
    dataset: String,
    request: String,
    fingerprint: String,
}

impl<T: Serialize> Command<T> {
    pub fn key(&self, operation: &str) -> Result<Key> {
        if self.request.len() != 36
            || !self
                .request
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        {
            return Err(Rejected::Invalid("Invalid capture request identity.".into()).into());
        }
        let mut hash = blake3::Hasher::new();
        hash.update(operation.as_bytes());
        serde_json::to_writer(&mut hash, &self.input)?;
        Ok(Key {
            dataset: self.dataset.clone(),
            request: self.request.clone(),
            fingerprint: hash.finalize().to_hex().to_string(),
        })
    }
}

impl Key {
    pub fn delivered(&self, database: &Path) -> Result<Option<Value>> {
        self.receipt(&db::Database::read(database)?)
    }

    fn receipt(&self, connection: &Connection) -> Result<Option<Value>> {
        if db::dataset(connection)? != self.dataset {
            return Err(Rejected::Dataset.into());
        }
        let row: Option<(String, String)> = connection
            .query_row(
                "SELECT fingerprint,reply FROM tactic_capture_receipts WHERE command=?1",
                [&self.request],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(fingerprint, reply)| {
            if fingerprint != self.fingerprint {
                return Err(Rejected::Conflict.into());
            }
            Ok(serde_json::from_str(&reply)?)
        })
        .transpose()
    }

    pub fn apply(
        &self,
        database: &mut Connection,
        prepared: &tactic_import::Prepared,
    ) -> Result<Value> {
        let tx = database.transaction()?;
        if let Some(reply) = self.receipt(&tx)? {
            return Ok(reply);
        }
        let puzzle =
            catalog::apply_authored_tactic(&tx, &prepared.plan, prepared.source_url.as_deref())?;
        tx.execute(
            "UPDATE cards SET capture_order=(SELECT sequence+1 FROM daily_clock)
             WHERE id=?1 AND capture_order IS NULL AND quarantined=0 AND retired=0
               AND NOT EXISTS(SELECT 1 FROM learning WHERE card_id=?1)",
            [&puzzle.id],
        )?;
        let reply = json!({"id":puzzle.id,"puzzle":puzzle,"request":self.request});
        tx.execute(
            "INSERT INTO tactic_capture_receipts(command,fingerprint,card_id,reply) VALUES (?1,?2,?3,?4)",
            params![self.request, self.fingerprint, puzzle.id, serde_json::to_string(&reply)?],
        )?;
        tx.commit()?;
        Ok(reply)
    }
}

#[derive(Deserialize)]
struct LichessReply {
    game: LichessGame,
    puzzle: LichessPuzzle,
}

#[derive(Deserialize)]
struct LichessGame {
    pgn: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LichessPuzzle {
    id: String,
    initial_ply: usize,
    fen: Option<String>,
    solution: Vec<String>,
}

fn prepare_lichess(input: &LichessInput, reply: LichessReply) -> Result<tactic_import::Prepared> {
    let puzzle = reply.puzzle;
    ensure!(
        puzzle.id == input.id,
        "Lichess returned a different puzzle."
    );
    ensure!(
        !puzzle.solution.is_empty() && puzzle.solution.len() <= chess::MAX_PLIES,
        "Invalid Lichess solution length."
    );
    let game = chess::parse_pgn(&reply.game.pgn, false, 1)?.remove(0)?;
    let plies = game.main.moves.len();
    ensure!(
        plies.checked_sub(1) == Some(puzzle.initial_ply),
        "Lichess setup ply disagrees with its game."
    );
    let board = chess::board_at(&game.main, plies)?;
    if let Some(fen) = puzzle.fen {
        ensure!(
            chess::position_id(&chess::position(&fen)?) == chess::position_id(&board),
            "Lichess setup position disagrees with its game."
        );
    }
    let route = chess::validate_solver_route(chess::Route {
        fen: chess::fen(&board),
        moves: puzzle.solution,
    })?;
    tactic_import::prepare_route(
        input.name.as_deref(),
        &format!("Lichess puzzle {}", input.id),
        &route,
        &route.moves[..1],
        Some(format!("https://lichess.org/training/{}", input.id)),
    )
}

pub const LOOKUP_DEADLINE: Duration = Duration::from_secs(10);
const MAX_LICHESS_REPLY: usize = 128 * 1024;

pub async fn download_lichess(input: &LichessInput) -> Result<tactic_import::Prepared> {
    if !tactic_import::lichess_id(&input.id) {
        return Err(Rejected::Invalid("Enter a five-character Lichess puzzle ID.".into()).into());
    }
    resolve_lichess(input)
        .await
        .map_err(|error| Rejected::Lookup(error.to_string()).into())
}

async fn resolve_lichess(input: &LichessInput) -> Result<tactic_import::Prepared> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(LOOKUP_DEADLINE)
        .build()?;
    let mut response = client
        .get(format!("https://lichess.org/api/puzzle/{}", input.id))
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::USER_AGENT, "Tempo-Lean/puzzle-capture")
        .send()
        .await
        .context("Could not reach Lichess. Try again.")?;
    ensure!(
        response.status().is_success(),
        "Could not load the Lichess puzzle ({}).",
        response.status().as_u16()
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("Could not read the Lichess puzzle. Try again.")?
    {
        ensure!(
            bytes.len() + chunk.len() <= MAX_LICHESS_REPLY,
            "Lichess returned an oversized puzzle."
        );
        bytes.extend(&chunk);
    }
    let reply = serde_json::from_slice(&bytes).context("Lichess returned invalid puzzle data.")?;
    prepare_lichess(input, reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_lichess_payload_starts_after_setup_and_retains_all_solution_moves() -> Result<()> {
        const RESPONSE: &str = include_str!("../tests/fixtures/lichess-capture-PSjmf.json");
        let arena = tempfile::tempdir()?;
        let mut db = db::Database::init(&arena.path().join("capture.sqlite3"))?;
        let command = Command {
            dataset: db::dataset(&db)?,
            request: "00000000-0000-4000-8000-000000000001".to_owned(),
            input: LichessInput {
                id: "PSjmf".into(),
                name: None,
            },
        };
        let key = command.key("capture-lichess-id-v1")?;
        let prepared = prepare_lichess(&command.input, serde_json::from_str(RESPONSE)?)?;
        let reply = key.apply(&mut db, &prepared)?;
        assert_eq!(
            reply["puzzle"]["source_url"],
            "https://lichess.org/training/PSjmf"
        );
        assert_eq!(
            reply["puzzle"]["route"]["fen"],
            "5rk1/p4p2/5qpR/1R1Q4/3PK3/4P1P1/P1r2PP1/8 b - - 2 27"
        );
        assert_eq!(
            reply["puzzle"]["route"]["moves"],
            json!(["g8g7", "d5e5", "f6e5"])
        );
        assert_eq!(key.receipt(&db)?, Some(reply));
        let mismatch = LichessInput {
            id: "00008".into(),
            name: None,
        };
        assert!(prepare_lichess(&mismatch, serde_json::from_str(RESPONSE)?).is_err());
        let mut invalid: LichessReply = serde_json::from_str(RESPONSE)?;
        invalid.puzzle.solution[0] = "a1a8".into();
        assert!(prepare_lichess(&command.input, invalid).is_err());
        Ok(())
    }
}
