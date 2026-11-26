use crate::{chess, db};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;
use std::{collections::BTreeMap, path::Path};

pub fn import(db: &mut Connection, file: &Path) -> Result<ImportReport> {
    use std::io::Read;
    const MAX_ARTIFACT: usize = 128 * 1024 * 1024;
    let mut bytes = Vec::new();
    std::fs::File::open(file)?
        .take(MAX_ARTIFACT as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_ARTIFACT,
        "Lichess catalog exceeds 128 MiB"
    );
    ensure!(
        bytes.starts_with(b"SQLite format 3\0"),
        "Expected a SQLite catalog"
    );
    let artifact = blake3::hash(&bytes).to_hex().to_string();
    use std::io::Write;
    let mut copy = tempfile::NamedTempFile::new()?;
    copy.write_all(&bytes)?;
    let snapshot = Connection::open_with_flags(copy.path(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    snapshot.execute_batch("PRAGMA trusted_schema=OFF;")?;
    let format: String =
        snapshot.query_row("SELECT value FROM metadata WHERE key='format'", [], |r| {
            r.get(0)
        })?;
    ensure!(format == "1", "Unsupported Lichess catalog format");
    let source: String = snapshot.query_row(
        "SELECT value FROM metadata WHERE key='source_sha256'",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        source.len() == 64 && source.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid source checksum"
    );
    let mut puzzles = BTreeMap::new();
    let mut query = snapshot.prepare("SELECT puzzle_id,rating,source_fen,source_moves,solver_fen,tags FROM puzzles ORDER BY puzzle_id")?;
    for row in query.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
        ))
    })? {
        let (id, rating, fen, moves, solver, tags) = row?;
        ensure!(
            id.len() == 5 && id.bytes().all(|b| b.is_ascii_alphanumeric()) && rating >= 1000,
            "Invalid puzzle identity or rating"
        );
        let route = chess::Route {
            fen,
            moves: moves.split_whitespace().map(str::to_owned).collect(),
        };
        ensure!(route.moves.len() >= 2, "Missing solver move for {id}");
        let solver_fen = chess::fen(&chess::board_at(&route, 1)?);
        ensure!(
            solver_fen == chess::fen(&chess::position(&solver)?),
            "Setup disagrees for {id}"
        );
        chess::board_at(&route, route.moves.len())
            .with_context(|| format!("Invalid line for {id}"))?;
        let solution = serde_json::to_string(&route.moves[1..])?;
        let version = chess::id(&["lichess-solution-v1", &solver_fen, &solution]);
        ensure!(
            puzzles
                .insert(id, (rating, solver_fen, solution, version, tags))
                .is_none(),
            "Duplicate puzzle"
        );
    }
    let mut packs = Vec::new();
    let mut query = snapshot
        .prepare("SELECT theme,rating_min,rating_end FROM packs ORDER BY theme,rating_min")?;
    let mut previous: Option<(String, Option<i64>)> = None;
    for row in query.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<i64>>(2)?,
        ))
    })? {
        let (theme, band, ceiling) = row?;
        let expected = match &previous {
            Some((key, end)) if *key == theme => *end,
            _ => Some(1000),
        };
        ensure!(
            Some(band) == expected
                && band.checked_add(100).is_some()
                && ceiling.is_none_or(|end| end == band + 100)
                && !theme.is_empty()
                && theme.len() <= 40
                && theme.bytes().all(|b| b.is_ascii_alphanumeric()),
            "Invalid pack band for {theme}"
        );
        let mut members=snapshot.prepare("SELECT position,puzzle_id FROM pack_puzzles WHERE theme=?1 AND rating_min=?2 ORDER BY position")?;
        let members = members
            .query_map(params![theme, band], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(members.len() == 20, "Incomplete {theme} pack at {band}");
        let mut last_rating = band;
        for (index, (position, id)) in members.iter().enumerate() {
            let (rating, _, _, _, tags) = puzzles.get(id).context("Missing pack member")?;
            ensure!(
                *position == index as i64 + 1
                    && *rating >= last_rating
                    && ceiling.is_none_or(|end| *rating < end)
                    && (theme == "mix" || tags.split_whitespace().any(|tag| tag == theme)),
                "Invalid member of {theme} pack at {band}"
            );
            last_rating = *rating;
        }
        previous = Some((theme.clone(), ceiling));
        packs.push((theme, band, ceiling, members));
    }
    ensure!(!packs.is_empty(), "Catalog contains no packs");
    let report = ImportReport {
        packs: packs.len(),
        puzzles: puzzles.len(),
        artifact: artifact.clone(),
    };
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if revision(&tx)?.as_deref() == Some(&artifact) {
        return Ok(report);
    }
    tx.execute("DELETE FROM lichess_packs", [])?;
    for (id, (_, fen, moves, version, _)) in &puzzles {
        tx.execute(
            "INSERT OR IGNORE INTO lichess_versions VALUES (?1,?2,?3,?4)",
            params![id, version, fen, moves],
        )?;
    }
    for (theme, band, ceiling, members) in packs {
        tx.execute(
            "INSERT INTO lichess_packs VALUES (?1,?2,?3)",
            params![theme, band, ceiling],
        )?;
        for (position, id) in members {
            let (rating, _, _, version, _) = &puzzles[&id];
            tx.execute(
                "INSERT INTO lichess_members VALUES (?1,?2,?3,?4,?5,?6)",
                params![theme, band, position, id, version, rating],
            )?;
        }
    }
    tx.execute("INSERT INTO lichess_catalog VALUES (1,?1,?2) ON CONFLICT(singleton) DO UPDATE SET artifact=excluded.artifact,source=excluded.source",params![artifact,source])?;
    tx.commit()?;
    Ok(report)
}

#[derive(Serialize)]
pub struct ImportReport {
    packs: usize,
    puzzles: usize,
    artifact: String,
}

fn revision(db: &Connection) -> Result<Option<String>> {
    Ok(db
        .query_row(
            "SELECT artifact FROM lichess_catalog WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()?)
}

#[derive(Serialize)]
pub struct Index {
    dataset: String,
    revision: Option<String>,
    themes: Vec<Theme>,
}
#[derive(Serialize)]
struct Theme {
    key: String,
    packs: usize,
}
pub fn index(db: &Connection) -> Result<Index> {
    let mut q = db.prepare(
        "SELECT theme,count(*) FROM lichess_packs GROUP BY theme ORDER BY theme LIMIT 200",
    )?;
    let themes = q
        .query_map([], |r| {
            Ok(Theme {
                key: r.get(0)?,
                packs: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Index {
        dataset: db::dataset(db)?,
        revision: revision(db)?,
        themes,
    })
}
#[derive(Serialize)]
pub struct ThemePacks {
    revision: Option<String>,
    packs: Vec<Pack>,
}
#[derive(Serialize)]
struct Pack {
    band: i64,
    ceiling: Option<i64>,
    done: usize,
}
pub fn theme(db: &Connection, key: &str, after: i64) -> Result<ThemePacks> {
    let mut q=db.prepare("SELECT p.band,p.ceiling,(SELECT count(*) FROM lichess_members m JOIN lichess_cards c ON c.puzzle=m.puzzle WHERE m.theme=p.theme AND m.band=p.band) FROM lichess_packs p WHERE p.theme=?1 AND p.band>?2 ORDER BY p.band LIMIT 50")?;
    let packs = q
        .query_map(params![key, after], |r| {
            Ok(Pack {
                band: r.get(0)?,
                ceiling: r.get(1)?,
                done: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ThemePacks {
        revision: revision(db)?,
        packs,
    })
}
#[derive(Serialize)]
pub struct Members {
    revision: Option<String>,
    puzzles: Vec<Member>,
}
#[derive(Serialize)]
struct Member {
    puzzle: String,
    rating: i64,
    done: bool,
    resume: bool,
}
pub fn members(db: &Connection, key: &str, band: i64) -> Result<Members> {
    let mut q=db.prepare("SELECT m.puzzle,m.rating,c.card IS NOT NULL,EXISTS(SELECT 1 FROM lichess_plays l WHERE l.puzzle=m.puzzle AND l.mode='first' AND l.result IS NULL) FROM lichess_members m LEFT JOIN lichess_cards c ON c.puzzle=m.puzzle WHERE m.theme=?1 AND m.band=?2 ORDER BY m.position")?;
    let puzzles = q
        .query_map(params![key, band], |r| {
            Ok(Member {
                puzzle: r.get(0)?,
                rating: r.get(1)?,
                done: r.get(2)?,
                resume: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Members {
        revision: revision(db)?,
        puzzles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        lichess_play::{self, Surface},
        study::Conflict,
    };

    fn artifact(path: &Path, first_move: &str, start_id: i64, size: i64) -> Result<()> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "CREATE TABLE metadata(key,value); CREATE TABLE packs(theme,rating_min,rating_end);
            CREATE TABLE pack_puzzles(theme,rating_min,position,puzzle_id);
            CREATE TABLE puzzles(puzzle_id,rating,source_fen,source_moves,solver_fen,tags);",
        )?;
        db.execute(
            "INSERT INTO metadata VALUES ('format','1'),('source_sha256',?1)",
            ["a".repeat(64)],
        )?;
        db.execute("INSERT INTO packs VALUES ('fork',1000,1100)", [])?;
        let source = chess::Route {
            fen: chess::fen(&chess::position("")?),
            moves: vec!["e2e4".into()],
        };
        let solver = chess::fen(&chess::board_at(&source, 1)?);
        for offset in 0..size {
            let id = format!("{:05}", offset + start_id);
            db.execute(
                "INSERT INTO puzzles VALUES (?1,1000,?2,?3,?4,'fork')",
                params![id, source.fen, format!("e2e4 {first_move} g1f3"), solver],
            )?;
            db.execute(
                "INSERT INTO pack_puzzles VALUES ('fork',1000,?1,?2)",
                params![offset + 1, id],
            )?;
        }
        Ok(())
    }

    #[test]
    fn publication_preserves_issued_answers_and_never_admits_by_import() -> Result<()> {
        let files = tempfile::tempdir()?;
        let base = files.path().join("base?#.sqlite3");
        let next = files.path().join("refresh.sqlite3");
        let invalid = files.path().join("invalid.sqlite3");
        artifact(&base, "e7e5", 0, 20)?;
        artifact(&next, "c7c5", 1, 20)?;
        artifact(&invalid, "c7c5", 1, 19)?;
        let mut db = db::Database::init(&files.path().join("tempo.sqlite3"))?;
        let first = import(&mut db, &base)?;
        assert_eq!(import(&mut db, &base)?.artifact, first.artifact);
        let dataset = db::dataset(&db)?;
        let start = |puzzle: &str, request: &str, revision: &str| {
            serde_json::from_value(serde_json::json!({
                "puzzle":puzzle,"request":request,"revision":revision,"dataset":dataset,"practice":false,
            }))
        };
        let removed = lichess_play::start(
            &mut db,
            start(
                "00000",
                "00000000-0000-0000-0000-000000000000",
                &first.artifact,
            )?,
        )?;
        let changed = lichess_play::start(
            &mut db,
            start(
                "00001",
                "00000000-0000-0000-0000-000000000001",
                &first.artifact,
            )?,
        )?;
        assert!(import(&mut db, &invalid).is_err());
        assert_eq!(revision(&db)?, Some(first.artifact));
        import(&mut db, &next)?;
        assert_eq!(
            db.query_row("SELECT count(*) FROM cards", [], |r| r.get::<_, i64>(0))?,
            0
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM learning", [], |r| r.get::<_, i64>(0))?,
            0
        );
        assert_eq!(lichess_play::pending(&db, 0)?.len(), 2);
        assert_eq!(
            lichess_play::resume_tactics(&db, removed.play)?.fen,
            removed.fen
        );
        let answer = |played: &str| lichess_play::Move {
            dataset: dataset.clone(),
            play: changed.play,
            step: 0,
            played: Some(played.into()),
        };
        let solved = lichess_play::play(&mut db, answer("e7e5"), Surface::Tactics)?;
        assert_eq!(serde_json::to_value(solved.view.result)?["rating"], "Good");
        assert_eq!(
            db.query_row("SELECT count(*) FROM learning", [], |r| r.get::<_, i64>(0))?,
            1
        );
        assert!(matches!(
            lichess_play::play(&mut db, answer("c7c5"), Surface::Tactics)
                .err()
                .unwrap()
                .downcast_ref::<Conflict>(),
            Some(Conflict::Submission)
        ));
        Ok(())
    }
}
