//! Read-only similarity evidence for one repertoire's authored replies.
use crate::{catalog, chess, db};
use pgn_reader::shakmaty::{ByColor, ByRole, Chess, Position, Role, san::SanPlus};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Read};

const MAX_REPLIES: usize = 16_384;
const MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Error {
    InvalidRequest,
    InvalidSnapshot,
    WorkLimit,
    Unavailable,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

pub struct PositionIr(Chess);

impl PositionIr {
    pub fn parse(fen: &str) -> Result<Self, Error> {
        if fen.len() > 256 || fen.split_ascii_whitespace().count() != 6 {
            return Err(Error::InvalidRequest);
        }
        chess::position(fen)
            .map(Self)
            .map_err(|_| Error::InvalidRequest)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Basis {
    dataset: String,
    repertoire_id: String,
    color: String,
    reply_revision: i64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepertoireIr {
    schema_version: u8,
    basis: Basis,
    replies: Vec<Reply>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    fen: String,
    movement: String,
    answer_version: String,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    count: usize,
}
impl Default for Options {
    fn default() -> Self {
        Self { count: 5 }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    fen: String,
    repertoire: RepertoireIr,
    #[serde(default)]
    options: Options,
}

#[derive(Serialize)]
pub struct Analysis {
    position_id: String,
    basis: Basis,
    ranking_version: u8,
    #[serde(flatten)]
    outcome: Outcome,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Outcome {
    Ranked { moves: Vec<Candidate> },
    OpponentToMove,
    Terminal { outcome: Terminal },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Terminal {
    Checkmate { winner: &'static str },
    Stalemate,
}

#[derive(Serialize)]
struct Candidate {
    first_move: String,
    san: String,
    distance: u32,
    example: Example,
}

#[derive(Serialize)]
struct Example {
    position_id: String,
    fen: String,
    answer_version: String,
}

fn distance(position: &Chess, example: &Chess) -> u32 {
    let pieces: u32 = ByColor::new_with(|color| {
        ByRole::new_with(|role| {
            let a = position.board().by_piece(role.of(color));
            let b = example.board().by_piece(role.of(color));
            (a ^ b).count() as u32 * if role == Role::Pawn { 3 } else { 1 }
        })
        .into_iter()
        .sum::<u32>()
    })
    .into_iter()
    .sum();
    pieces
        + (position.castles().castling_rights() ^ example.castles().castling_rights()).count()
            as u32
        + u32::from(position.legal_ep_square() != example.legal_ep_square())
}

pub fn analyze(
    position: PositionIr,
    repertoire: RepertoireIr,
    options: Options,
) -> Result<Analysis, Error> {
    analyze_until(position, repertoire, options, || false)
}

pub fn analyze_until(
    PositionIr(board): PositionIr,
    repertoire: RepertoireIr,
    options: Options,
    cancelled: impl Fn() -> bool,
) -> Result<Analysis, Error> {
    if !(1..=10).contains(&options.count) {
        return Err(Error::InvalidRequest);
    }
    if repertoire.schema_version != 1
        || !matches!(repertoire.basis.color.as_str(), "white" | "black")
        || repertoire.basis.dataset.is_empty()
        || repertoire.basis.repertoire_id.is_empty()
        || repertoire.basis.reply_revision < 0
    {
        return Err(Error::InvalidSnapshot);
    }
    if repertoire.replies.len() > MAX_REPLIES {
        return Err(Error::WorkLimit);
    }
    let legal: BTreeMap<_, _> = board
        .legal_moves()
        .into_iter()
        .map(|movement| (chess::uci(movement), movement))
        .collect();
    let mut best: BTreeMap<String, Candidate> = BTreeMap::new();
    let mut authored = BTreeMap::new();
    for mut reply in repertoire.replies {
        if cancelled() {
            return Err(Error::Unavailable);
        }
        let PositionIr(example) =
            PositionIr::parse(&reply.fen).map_err(|_| Error::InvalidSnapshot)?;
        let movement =
            chess::parse_move(&example, &reply.movement).map_err(|_| Error::InvalidSnapshot)?;
        if chess::color_name(example.turn()) != repertoire.basis.color
            || chess::uci(movement) != reply.movement
            || reply.answer_version.is_empty()
        {
            return Err(Error::InvalidSnapshot);
        }
        let position_id = chess::position_id(&example);
        let saved = (reply.movement.clone(), reply.answer_version.clone());
        if authored
            .insert(position_id.clone(), saved.clone())
            .is_some_and(|prev| prev != saved)
        {
            return Err(Error::InvalidSnapshot);
        }
        if board.turn() != example.turn() {
            continue;
        }
        let Some(&movement) = legal.get(&reply.movement) else {
            continue;
        };
        let score = distance(&board, &example);
        reply.fen = chess::fen(&example);
        if best.get(&reply.movement).is_some_and(|old| {
            (old.distance, &old.example.position_id, &old.example.fen)
                <= (score, &position_id, &reply.fen)
        }) {
            continue;
        }
        best.insert(
            reply.movement.clone(),
            Candidate {
                first_move: reply.movement,
                san: SanPlus::from_move(board.clone(), movement).to_string(),
                distance: score,
                example: Example {
                    position_id,
                    fen: reply.fen,
                    answer_version: reply.answer_version,
                },
            },
        );
    }
    let outcome = if legal.is_empty() {
        Outcome::Terminal {
            outcome: if board.is_check() {
                Terminal::Checkmate {
                    winner: chess::color_name(!board.turn()),
                }
            } else {
                Terminal::Stalemate
            },
        }
    } else if chess::color_name(board.turn()) != repertoire.basis.color {
        Outcome::OpponentToMove
    } else {
        let mut moves: Vec<_> = best.into_values().collect();
        moves.sort_by(|a, b| (a.distance, &a.first_move).cmp(&(b.distance, &b.first_move)));
        moves.truncate(options.count);
        Outcome::Ranked { moves }
    };
    Ok(Analysis {
        position_id: chess::position_id(&board),
        basis: repertoire.basis,
        ranking_version: 1,
        outcome,
    })
}

pub fn snapshot(db: &Connection, repertoire_id: &str, color: &str) -> anyhow::Result<RepertoireIr> {
    catalog::existing_repertoire(db, repertoire_id, color).map_err(|error| {
        if error.is::<catalog::MissingRepertoire>() {
            Error::InvalidRequest.into()
        } else {
            error
        }
    })?;
    let basis = Basis {
        dataset: db::dataset(db)?,
        repertoire_id: repertoire_id.into(),
        color: color.into(),
        reply_revision: catalog::authored_reply_revision(db, repertoire_id)?,
    };
    let mut query = db.prepare(
        "SELECT fen,answers,answer_version FROM cards
         WHERE kind='opening' AND opening_retired=0 AND quarantined=0 AND repertoire_id=?1 AND color=?2
         ORDER BY position_id LIMIT ?3",
    )?;
    let mut rows = query.query((repertoire_id, color, MAX_REPLIES + 1))?;
    let mut replies = Vec::new();
    while let Some(row) = rows.next()? {
        if replies.len() == MAX_REPLIES {
            return Err(Error::WorkLimit.into());
        }
        let fen = row.get_ref(0)?.as_str()?;
        let answers = row.get_ref(1)?.as_str()?;
        let answer_version = row.get_ref(2)?.as_str()?;
        // Canonical rows fit below the byte budget even at the row limit.
        if fen.len() > 256
            || answers.len() > 32
            || answer_version.len() > 128
            || ![fen, answer_version].into_iter().all(|value| {
                value.bytes().all(|byte| {
                    byte == b' ' || (byte.is_ascii_graphic() && !matches!(byte, b'"' | b'\\'))
                })
            })
        {
            return Err(Error::InvalidSnapshot.into());
        }
        let [movement]: [String; 1] =
            serde_json::from_str(answers).map_err(|_| Error::InvalidSnapshot)?;
        replies.push(Reply {
            fen: fen.into(),
            movement,
            answer_version: answer_version.into(),
        });
    }
    if basis.dataset.len() > 256 || basis.repertoire_id.len() > 256 {
        return Err(Error::InvalidSnapshot.into());
    }
    Ok(RepertoireIr {
        schema_version: 1,
        basis,
        replies,
    })
}

pub fn export(
    path: &std::path::Path,
    repertoire: &str,
    color: &str,
) -> anyhow::Result<RepertoireIr> {
    let db = db::Database::read(path)?;
    let tx = db.unchecked_transaction()?;
    snapshot(&tx, repertoire, color)
}

pub fn cli(input: impl Read) -> Result<Analysis, Error> {
    let mut bytes = Vec::new();
    input
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::InvalidRequest)?;
    if bytes.len() > MAX_BYTES {
        return Err(Error::WorkLimit);
    }
    let Request {
        fen,
        repertoire,
        options,
    } = serde_json::from_slice(&bytes).map_err(|_| Error::InvalidRequest)?;
    analyze(PositionIr::parse(&fen)?, repertoire, options)
}
