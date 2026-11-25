use crate::chess::{self, Route};
use anyhow::{Context, Result, ensure};
use pgn_reader::shakmaty::{Chess, Color, Position};
use pgn_reader::{Nag, Outcome, RawComment, RawTag, Reader, SanPlus, Skip, Visitor};
use serde::Serialize;
use std::{fmt::Write, ops::ControlFlow};

pub fn numbered_move(board: &Chess, san: &str) -> String {
    let dots = if board.turn() == Color::White {
        "."
    } else {
        "..."
    };
    format!("{}{dots} {san}", board.fullmoves())
}

pub fn reply_text(board: &Chess, reply: &str) -> Result<String> {
    Ok(numbered_move(
        board,
        &SanPlus::from_move(board.clone(), chess::parse_move(board, reply)?).to_string(),
    ))
}

pub fn line_text(route: &Route, position: &str, reply: &str) -> Result<String> {
    let mut board = chess::position(&route.fen)?;
    let mut output = String::new();
    for (ply, played) in route
        .moves
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(reply))
        .enumerate()
    {
        if ply == route.moves.len() {
            ensure!(
                chess::position_id(&board) == position,
                "route does not reach this position"
            );
        }
        let movement = chess::parse_move(&board, played)?;
        let san = SanPlus::from_move(board.clone(), movement).to_string();
        if ply > 0 {
            output.push(' ');
        }
        if ply == 0 || board.turn() == Color::White {
            output.push_str(&numbered_move(&board, &san));
        } else {
            output.push_str(&san);
        }
        board.play_unchecked(movement);
    }
    Ok(output)
}

#[derive(Serialize)]
pub struct Trim {
    pub line: String,
    pub first_removed: String,
}

#[derive(Serialize)]
pub struct Correction {
    pub pgn: String,
    pub trims: Vec<Trim>,
}

struct Node {
    parent: usize,
    board: Chess,
    played: String,
    annotations: Vec<String>,
    children: Vec<usize>,
}

struct Document {
    tags: Vec<(String, String)>,
    nodes: Vec<Node>,
    current: usize,
    stack: Vec<usize>,
    result: String,
    partial: String,
}

struct ReadDocument;

impl Visitor for ReadDocument {
    type Tags = Vec<(String, String)>;
    type Movetext = Document;
    type Output = Result<Document>;

    fn begin_tags(&mut self) -> ControlFlow<Self::Output, Self::Tags> {
        ControlFlow::Continue(vec![])
    }

    fn tag(
        &mut self,
        tags: &mut Self::Tags,
        name: &[u8],
        value: RawTag<'_>,
    ) -> ControlFlow<Self::Output> {
        tags.push((
            String::from_utf8_lossy(name).into_owned(),
            String::from_utf8_lossy(value.as_bytes()).into_owned(),
        ));
        ControlFlow::Continue(())
    }

    fn begin_movetext(&mut self, tags: Self::Tags) -> ControlFlow<Self::Output, Document> {
        let setup = tags
            .iter()
            .rev()
            .find(|(name, _)| name == "FEN")
            .map_or("", |(_, fen)| fen);
        let board = match chess::position(setup) {
            Ok(board) => board,
            Err(error) => return ControlFlow::Break(Err(error)),
        };
        ControlFlow::Continue(Document {
            tags,
            nodes: vec![Node {
                parent: 0,
                board,
                played: String::new(),
                annotations: vec![],
                children: vec![],
            }],
            current: 0,
            stack: vec![],
            result: "*".into(),
            partial: String::new(),
        })
    }

    fn san(&mut self, doc: &mut Document, san: SanPlus) -> ControlFlow<Self::Output> {
        let mut board = doc.nodes[doc.current].board.clone();
        let movement = match san.san.to_move(&board) {
            Ok(movement) => movement,
            Err(error) => return ControlFlow::Break(Err(error.into())),
        };
        board.play_unchecked(movement);
        let next = doc.nodes.len();
        doc.nodes[doc.current].children.push(next);
        doc.nodes.push(Node {
            parent: doc.current,
            board,
            played: chess::uci(movement),
            annotations: vec![],
            children: vec![],
        });
        doc.current = next;
        ControlFlow::Continue(())
    }

    fn begin_variation(&mut self, doc: &mut Document) -> ControlFlow<Self::Output, Skip> {
        doc.stack.push(doc.current);
        doc.current = doc.nodes[doc.current].parent;
        ControlFlow::Continue(Skip(false))
    }

    fn end_variation(&mut self, doc: &mut Document) -> ControlFlow<Self::Output> {
        if let Some(parent) = doc.stack.pop() {
            doc.current = parent;
        }
        ControlFlow::Continue(())
    }

    fn nag(&mut self, doc: &mut Document, nag: Nag) -> ControlFlow<Self::Output> {
        doc.nodes[doc.current].annotations.push(nag.to_string());
        ControlFlow::Continue(())
    }

    fn partial_comment(
        &mut self,
        doc: &mut Document,
        comment: RawComment<'_>,
    ) -> ControlFlow<Self::Output> {
        doc.partial
            .push_str(&String::from_utf8_lossy(comment.as_bytes()));
        ControlFlow::Continue(())
    }

    fn comment(
        &mut self,
        doc: &mut Document,
        comment: RawComment<'_>,
    ) -> ControlFlow<Self::Output> {
        let prefix = std::mem::take(&mut doc.partial);
        doc.nodes[doc.current].annotations.push(format!(
            "{{{prefix}{}}}",
            String::from_utf8_lossy(comment.as_bytes())
        ));
        ControlFlow::Continue(())
    }

    fn outcome(&mut self, doc: &mut Document, outcome: Outcome) -> ControlFlow<Self::Output> {
        if doc.stack.is_empty() {
            doc.result = outcome.to_string();
        }
        ControlFlow::Continue(())
    }

    fn end_game(&mut self, doc: Document) -> Self::Output {
        Ok(doc)
    }
}

struct Repair<'a> {
    doc: &'a Document,
    anchor: &'a str,
    position: &'a str,
    reply: &'a str,
    trims: Vec<Trim>,
    changes: usize,
}

impl Repair<'_> {
    fn children(
        &mut self,
        parent: usize,
        board: Chess,
        mut reached: bool,
        output: &mut String,
    ) -> Result<()> {
        let mut parent = parent;
        let mut board = board;
        loop {
            let node = &self.doc.nodes[parent];
            let original_position = chess::position_id(&node.board);
            reached |= original_position == self.anchor;
            let replace = reached
                && original_position == self.position
                && chess::position_id(&board) == self.position;
            let mut legal = Vec::new();
            for &index in &node.children {
                let original = &self.doc.nodes[index];
                let played = if replace {
                    self.reply
                } else {
                    &original.played
                };
                match chess::parse_move(&board, played) {
                    Ok(movement) => {
                        self.changes += usize::from(played != original.played);
                        let mut after = board.clone();
                        let san =
                            SanPlus::from_move_and_play_unchecked(&mut after, movement).to_string();
                        legal.push((index, after, numbered_move(&board, &san)));
                    }
                    Err(_) => {
                        let mut prefix = Vec::new();
                        let mut cursor = parent;
                        while cursor != 0 {
                            prefix.push(self.doc.nodes[cursor].played.clone());
                            cursor = self.doc.nodes[cursor].parent;
                        }
                        prefix.reverse();
                        self.trims.push(Trim {
                            line: line_text(
                                &Route {
                                    fen: chess::fen(&self.doc.nodes[0].board),
                                    moves: prefix,
                                },
                                &original_position,
                                &original.played,
                            )?,
                            first_removed: reply_text(&node.board, &original.played)?,
                        });
                    }
                }
            }
            let mut legal = legal.into_iter();
            let Some((main, after, text)) = legal.next() else {
                return Ok(());
            };
            self.write_move(main, &text, output);
            for (variation, variation_board, text) in legal {
                output.push_str(" (");
                self.write_move(variation, &text, output);
                self.children(variation, variation_board, reached, output)?;
                output.push(')');
            }
            parent = main;
            board = after;
        }
    }

    fn write_move(&self, node: usize, text: &str, output: &mut String) {
        output.push(' ');
        output.push_str(text);
        for annotation in &self.doc.nodes[node].annotations {
            output.push(' ');
            output.push_str(annotation);
        }
    }
}

// The PGN visitor reports brace comments only. Represent line comments as
// temporary brace comments so the same visitor can attach them to the right move.
fn capture_line_comments(source: &str) -> (String, String, Vec<String>) {
    let mut prefix = "TempoLineComment".to_owned();
    while source.contains(&prefix) {
        prefix.push('_');
    }
    let mut normalized = String::with_capacity(source.len());
    let mut comments = Vec::new();
    let mut input = source.char_indices().peekable();
    let mut quoted = false;
    let mut braced = false;
    while let Some((i, ch)) = input.next() {
        match ch {
            '\\' if quoted => {
                normalized.push(ch);
                if let Some((_, escaped)) = input.next() {
                    normalized.push(escaped);
                }
            }
            '"' if !braced => {
                quoted = !quoted;
                normalized.push(ch);
            }
            '{' if !quoted => {
                braced = true;
                normalized.push(ch);
            }
            '}' if !quoted => {
                braced = false;
                normalized.push(ch);
            }
            ';' if !quoted && !braced => {
                let end = source[i..]
                    .find('\n')
                    .map_or(source.len(), |length| i + length);
                while input.peek().is_some_and(|(index, _)| *index < end) {
                    input.next();
                }
                let marker = format!("{{{prefix}{}}}", comments.len());
                comments.push(format!("{}\n", &source[i..end]));
                normalized.push_str(&marker);
            }
            _ => normalized.push(ch),
        }
    }
    (normalized, format!("{{{prefix}"), comments)
}

pub fn correct(pgn: &str, anchor: &str, position: &str, reply: &str) -> Result<Correction> {
    let (with_comments, marker, comments) = capture_line_comments(pgn);
    let doc = Reader::new(with_comments.as_bytes())
        .read_game(&mut ReadDocument)?
        .context("PGN has no games")??;
    let mut repair = Repair {
        doc: &doc,
        anchor,
        position,
        reply,
        trims: vec![],
        changes: 0,
    };
    let mut output = String::new();
    for (name, value) in &doc.tags {
        writeln!(output, "[{name} \"{value}\"]")?;
    }
    if !doc.tags.is_empty() {
        output.push('\n');
    }
    for annotation in &doc.nodes[0].annotations {
        output.push_str(annotation);
        output.push(' ');
    }
    repair.children(0, doc.nodes[0].board.clone(), false, &mut output)?;
    ensure!(
        repair.changes > 0,
        "This conflict is no longer in the draft."
    );
    write!(output, " {}", doc.result)?;
    let mut restored = String::with_capacity(output.len());
    let mut segments = output.split(&marker);
    restored.push_str(segments.next().unwrap_or_default());
    for segment in segments {
        let (index, text) = segment
            .split_once('}')
            .context("Invalid PGN comment marker")?;
        restored.push_str(&comments[index.parse::<usize>()?]);
        restored.push_str(text);
    }
    let pgn = restored.trim().to_owned();
    ensure!(
        pgn.len() <= 128 * 1024,
        "The corrected PGN exceeds 128 KiB. Edit the draft directly."
    );
    chess::parse_gap_pgn(&pgn)?;
    Ok(Correction {
        pgn,
        trims: repair.trims,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrects_repeated_transposition_without_cutting_legal_siblings() -> Result<()> {
        let original = "[Event \"Practice\"]\n\n1.Nf3 Nc6 2.e4 e5 3.Nc3 Nf6 4.d4 {choice} ;line note with }\n (4.Bc4 a6 5.a3 $1) exd4 5.Nxd4 *";
        let game = chess::parse_gap_pgn(original)?;
        let anchor = chess::position_id(&chess::board_at(&game.main, 2)?);
        let conflict = chess::position_id(&chess::board_at(&game.main, 6)?);
        let result = correct(original, &anchor, &conflict, "f1b5")?;
        assert_eq!(result.trims.len(), 1);
        assert_eq!(result.trims[0].first_removed, "4... exd4");
        assert!(result.pgn.contains("{choice}"));
        assert!(result.pgn.contains(";line note with }"));
        assert!(result.pgn.contains("$1"));
        let corrected = chess::parse_gap_pgn(&result.pgn)?;
        assert_eq!(corrected.tags["Event"], "Practice");
        assert_eq!(corrected.main.moves.last().unwrap(), "f1b5");
        assert_eq!(
            &corrected.variations[0].moves[6..],
            ["f1b5", "a7a6", "a2a3"]
        );
        Ok(())
    }

    #[test]
    fn preserves_black_setup_and_standard_notation() -> Result<()> {
        let original = "[SetUp \"1\"]\n[FEN \"rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 7\"]\n\n7... c5 8.Nf3 *";
        let original_route = chess::parse_gap_pgn(original)?.main;
        let position = chess::position_id(&chess::board_at(&original_route, 0)?);
        let result = correct(original, &position, &position, "e7e5")?;
        assert!(result.trims.is_empty());
        assert!(result.pgn.contains("7... e5 8. Nf3"));
        assert!(result.pgn.contains("[SetUp \"1\"]"));
        Ok(())
    }
}
