//! Chess-validated, provider-neutral mainline. The format is local to Games storage.
use crate::chess::{self, MAX_PLIES, Route};
use anyhow::{Result, ensure};
use pgn_reader::shakmaty::Position;

const VERSION: u8 = 1;
const HEADER: usize = 5;
const MAX_FEN: usize = 160;
const PROMOTIONS: &[u8] = b" nbrq";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mainline {
    start: String,
    moves: Vec<u16>,
}

impl Mainline {
    pub fn validate(route: &Route) -> Result<Self> {
        ensure!(
            !route.moves.is_empty() && route.moves.len() <= MAX_PLIES,
            "invalid game length"
        );
        let mut board = chess::position(&route.fen)?;
        let start = chess::fen(&board);
        let mut moves = Vec::with_capacity(route.moves.len());
        for notation in &route.moves {
            let played = chess::parse_move(&board, notation)?;
            let uci = chess::uci(played);
            let bytes = uci.as_bytes();
            let square = |i| u16::from(bytes[i] - b'a') + 8 * u16::from(bytes[i + 1] - b'1');
            let promotion = bytes.get(4).map_or(0, |letter| {
                PROMOTIONS.iter().position(|item| item == letter).unwrap() as u16
            });
            moves.push(square(0) | (square(2) << 6) | (promotion << 12));
            board.play_unchecked(played);
        }
        Ok(Self { start, moves })
    }

    pub fn start(&self) -> &str {
        &self.start
    }

    pub fn plies(&self) -> usize {
        self.moves.len()
    }

    pub fn moves(&self) -> impl Iterator<Item = String> + '_ {
        self.moves.iter().map(|&code| {
            let square = |n| [b'a' + (n % 8) as u8, b'1' + (n / 8) as u8];
            let source = square(code & 63);
            let target = square((code >> 6) & 63);
            let mut movement = String::with_capacity(5);
            movement.extend([source, target].into_iter().flatten().map(char::from));
            if code >> 12 != 0 {
                movement.push(char::from(PROMOTIONS[usize::from(code >> 12)]));
            }
            movement
        })
    }

    pub fn route(&self) -> Route {
        Route {
            fen: self.start.clone(),
            moves: self.moves().collect(),
        }
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER + self.start.len() + 2 * self.moves.len());
        bytes.push(VERSION);
        bytes.extend((self.start.len() as u16).to_le_bytes());
        bytes.extend((self.moves.len() as u16).to_le_bytes());
        bytes.extend(self.start.as_bytes());
        bytes.extend(self.moves.iter().flat_map(|code| code.to_le_bytes()));
        bytes
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() >= HEADER && bytes[0] == VERSION,
            "unsupported archived game format"
        );
        let fen_len = usize::from(u16::from_le_bytes([bytes[1], bytes[2]]));
        let plies = usize::from(u16::from_le_bytes([bytes[3], bytes[4]]));
        ensure!(
            fen_len <= MAX_FEN
                && (1..=MAX_PLIES).contains(&plies)
                && bytes.len() == HEADER + fen_len + 2 * plies,
            "invalid archived game payload"
        );
        let start = std::str::from_utf8(&bytes[HEADER..HEADER + fen_len])?.to_owned();
        ensure!(
            chess::fen(&chess::position(&start)?) == start,
            "noncanonical archived starting position"
        );
        let moves = bytes[HEADER + fen_len..]
            .as_chunks::<2>()
            .0
            .iter()
            .copied()
            .map(u16::from_le_bytes)
            .collect::<Vec<_>>();
        ensure!(
            moves
                .iter()
                .all(|code| usize::from(code >> 12) < PROMOTIONS.len()),
            "invalid archived move code"
        );
        Ok(Self { start, moves })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orthodox_special_moves_round_trip() -> Result<()> {
        for (fen, moves) in [
            ("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1", vec!["e1g1", "e8c8"]),
            ("4k3/1P6/8/8/8/8/8/4K3 w - - 0 1", vec!["b7b8n"]),
            ("4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 1", vec!["e5d6"]),
        ] {
            let game = Mainline::validate(&Route {
                fen: fen.into(),
                moves: moves.into_iter().map(str::to_owned).collect(),
            })?;
            let mut encoded = game.encode();
            assert_eq!(Mainline::decode(&encoded)?, game);
            encoded[0] = 2;
            assert!(Mainline::decode(&encoded).is_err());
        }
        Ok(())
    }
}
