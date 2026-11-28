//! Publish or retire one addressed game finding through Tempo's catalog writer.
use crate::{
    catalog, chess, db,
    tactic_qualification::{self, Decision, Evidence},
    tactic_worker_protocol::{Failure, Work},
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior::Immediate, params};
use serde_json::{Value, json};

#[derive(Debug)]
pub struct InvalidEvidence(anyhow::Error);

impl std::fmt::Display for InvalidEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}
impl std::error::Error for InvalidEvidence {}

struct Candidate {
    work: Work,
    position: String,
    played: String,
    captures: Vec<String>,
    result: Option<String>,
}

fn candidate(db: &Connection, game: &str, ply: usize) -> Result<Option<Candidate>> {
    let row = db.query_row(
        "SELECT m.detector_version,q.policy_version,coalesce(m.fen,c.fen),m.position_id,m.played_move,m.captures,q.outcome
         FROM capture_qualification q JOIN missed_captures m USING(game_id,ply,detector_version)
         LEFT JOIN tactics t ON t.game_id=m.game_id AND t.game_ply=m.ply
         LEFT JOIN cards c ON c.id=t.id AND c.source_game_id=m.game_id AND c.source_ply=m.ply
           AND c.position_id=m.position_id AND c.played_move=m.played_move
         WHERE q.game_id=?1 AND q.ply=?2",
        params![game, ply],
        |r| Ok((r.get::<_, i64>(0)?,r.get::<_, i64>(1)?,r.get::<_, Option<String>>(2)?,
            r.get::<_, String>(3)?,r.get::<_, String>(4)?,r.get::<_, String>(5)?,r.get::<_, Option<String>>(6)?)),
    ).optional()?;
    let Some((version, policy, fen, position, played, captures, result)) = row else {
        return Ok(None);
    };
    let fen = fen.unwrap_or_default();
    let token = chess::id(&[
        &db::dataset(db)?,
        game,
        &ply.to_string(),
        &version.to_string(),
        &policy.to_string(),
        &fen,
        &position,
        &played,
        &captures,
    ]);
    Ok(Some(Candidate {
        work: Work {
            game: game.to_owned(),
            ply,
            token,
            fen,
            depth: tactic_qualification::DEPTH,
        },
        position,
        played,
        captures: serde_json::from_str(&captures)?,
        result,
    }))
}

pub fn next(db: &Connection, now: i64) -> Result<Option<Work>> {
    let key: Option<(String, usize)> = db
        .query_row(
            "SELECT game_id,ply FROM capture_qualification WHERE outcome IS NULL AND retry_at<=?1
         ORDER BY retry_at,game_id,ply LIMIT 1",
            [now],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((game, ply)) = key else {
        return Ok(None);
    };
    Ok(candidate(db, &game, ply)?.map(|candidate| candidate.work))
}

#[derive(Debug)]
pub struct StaleWork;
impl std::fmt::Display for StaleWork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Unknown or stale tactic work")
    }
}
impl std::error::Error for StaleWork {}

fn addressed(db: &Connection, work: &Work) -> Result<Candidate> {
    let candidate = candidate(db, &work.game, work.ply)?.ok_or(StaleWork)?;
    ensure!(
        work.token == candidate.work.token
            && work.fen == candidate.work.fen
            && work.depth == candidate.work.depth,
        StaleWork
    );
    Ok(candidate)
}

pub fn retry(db: &mut Connection, work: &Work, error: Failure, now: i64) -> Result<()> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let candidate = addressed(&tx, work)?;
    if candidate.result.is_none() {
        tx.execute(
            "UPDATE capture_qualification
             SET retry_at=?3+min(3600,60*(1<<min(failures,6))),failures=failures+1,last_error=?4
             WHERE game_id=?1 AND ply=?2 AND outcome IS NULL AND retry_at<=?3",
            params![
                work.game,
                work.ply,
                now,
                serde_json::to_value(error)?.as_str()
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn submit(db: &mut Connection, work: &Work, evidence: &Evidence) -> Result<String> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let candidate = addressed(&tx, work)?;
    if let Some(result) = candidate.result {
        return Ok(result);
    }
    let board = chess::position(&work.fen)?;
    ensure!(
        chess::position_id(&board) == candidate.position,
        "Capture position changed"
    );
    ensure!(
        chess::legal(&board).contains(&candidate.played),
        "Invalid played move"
    );
    let decision = tactic_qualification::qualify(&work.fen, &candidate.captures, evidence)
        .map_err(InvalidEvidence)?;
    let before = tx.total_changes();
    let outcome = match decision {
        Decision::Unique(best) if best != candidate.played => catalog::qualify_game_tactic(
            &tx,
            &work.game,
            work.ply,
            &work.fen,
            &candidate.played,
            Some(best),
        )?,
        rejection => {
            catalog::qualify_game_tactic(
                &tx,
                &work.game,
                work.ply,
                &work.fen,
                &candidate.played,
                None,
            )?;
            match rejection {
                Decision::EquivalentAlternatives => "equivalent",
                Decision::BestOutsideCandidates => "best_outside_candidates",
                Decision::Unique(_) => "played_best",
            }
        }
    };
    tx.execute(
        "UPDATE capture_qualification SET outcome=?3,evidence=?4,last_error=NULL
         WHERE game_id=?1 AND ply=?2",
        params![
            work.game,
            work.ply,
            outcome,
            serde_json::to_string(evidence)?
        ],
    )?;
    ensure!(
        tx.total_changes() - before <= catalog::ONLINE_WRITE_LIMIT as u64,
        "Capture qualification exceeds its transaction row budget"
    );
    tx.commit()?;
    Ok(outcome.to_owned())
}

pub fn status(db: &Connection) -> Result<Value> {
    Ok(db.query_row(
        "SELECT kept,narrowed,published,removed,rejected_unpublished,unresolved,retrying
         FROM capture_status WHERE singleton=1",
        [],
        |r| {
            Ok(json!({
                "kept":r.get::<_,u64>(0)?, "narrowed":r.get::<_,u64>(1)?,
                "published":r.get::<_,u64>(2)?, "removed":r.get::<_,u64>(3)?,
                "rejected_unpublished":r.get::<_,u64>(4)?, "unresolved":r.get::<_,u64>(5)?,
                "retrying":r.get::<_,u64>(6)?
            }))
        },
    )?)
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use crate::tactic_qualification::{Root, Score};

    pub fn fixture_evidence(work: &Work, best: &str) -> Result<Evidence> {
        let mut moves = chess::legal(&chess::position(&work.fen)?);
        moves.sort_by_key(|root| root != best);
        Ok(Evidence::Evaluated {
            engine: "fixture".into(),
            depth: work.depth,
            lines: moves
                .into_iter()
                .map(|first_move| Root {
                    score: Score::Centipawns {
                        white: if first_move == best { 31 } else { 0 },
                    },
                    first_move,
                })
                .collect(),
        })
    }

    pub fn publish_fixture(db: &mut Connection) -> Result<()> {
        let work = next(db, 0)?.unwrap();
        let best = candidate(db, &work.game, work.ply)?.unwrap().captures[0].clone();
        let evidence = fixture_evidence(&work, &best)?;
        db.execute_batch("CREATE TRIGGER fail_publish BEFORE INSERT ON tactics BEGIN SELECT raise(abort,'fixture failure'); END;")?;
        assert!(submit(db, &work, &evidence).is_err());
        assert_eq!(next(db, 0)?.unwrap().token, work.token);
        db.execute_batch("DROP TRIGGER fail_publish;")?;
        assert_eq!(submit(db, &work, &evidence)?, "published");
        assert_eq!(submit(db, &work, &evidence)?, "published");
        assert!(next(db, 0)?.is_none());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::fixture_evidence;
    use super::*;
    use crate::{
        db::Database,
        review,
        study::{AnswerAction, AnswerIntentV1, Next},
        study_store,
    };

    const FEN: &str = "4k3/8/8/n2n4/8/8/8/R2RK3 w - - 0 1";
    const A: &str = "a1a5";
    const B: &str = "d1d5";

    fn existing(db: &Connection, fen: Option<&str>) -> Result<String> {
        db.execute(
            "INSERT INTO games(id,white,black,learner_color,game_ir,occurrence,outcome) VALUES ('game','w','b','white',zeroblob(8),'fixture','*')",
            [],
        )?;
        let position = chess::position_id(&chess::position(FEN)?);
        db.execute(
            "INSERT INTO missed_captures VALUES ('game',0,?1,'e1e2',?2,?3,1)",
            params![position, serde_json::to_string(&[A, B])?, fen],
        )?;
        let plan = catalog::prepare_tactic(catalog::TacticInput {
            name: "Original name",
            route: &chess::Route {
                fen: FEN.into(),
                moves: vec![A.into()],
            },
            answers: &[A.into(), B.into()],
            source: Some(("game", 0, "e1e2")),
        })?;
        catalog::apply_tactic(db, &plan)
    }

    #[test]
    fn exact_status_is_constant_work_and_tracks_retry_and_verdict() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut db = Database::init(&directory.path().join("db"))?;
        existing(&db, Some(FEN))?;
        let work = next(&db, 0)?.unwrap();
        retry(&mut db, &work, Failure::Timeout, 0)?;
        assert_eq!(status(&db)?["retrying"], 1);
        let evidence = fixture_evidence(&work, A)?;
        assert_eq!(submit(&mut db, &work, &evidence)?, "narrowed");
        assert_eq!(submit(&mut db, &work, &evidence)?, "narrowed");
        assert_eq!(status(&db)?["retrying"], 0);
        assert_eq!(status(&db)?["narrowed"], 1);
        fn measure(db: &Connection) -> Result<(Value, usize)> {
            use std::sync::{
                Arc,
                atomic::{AtomicUsize, Ordering},
            };
            let steps = Arc::new(AtomicUsize::new(0));
            let hook = steps.clone();
            db.progress_handler(
                1,
                Some(move || {
                    hook.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            );
            let counts = status(db)?;
            db.progress_handler(0, None::<fn() -> bool>);
            Ok((counts, steps.load(Ordering::Relaxed)))
        }
        let (_, before) = measure(&db)?;
        db.execute_batch(
            "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<100000)
            INSERT INTO missed_captures
            SELECT 'game',i,'fixture','e1e2','[]',NULL,1 FROM n;
            UPDATE capture_qualification SET outcome='equivalent' WHERE ply>0;",
        )?;
        let (counts, after) = measure(&db)?;
        assert_eq!(counts["rejected_unpublished"], 100000);
        assert_eq!(counts["narrowed"], 1);
        assert_eq!(counts["unresolved"], 0);
        assert!(before.abs_diff(after) <= 2, "{before} versus {after}");
        Ok(())
    }

    #[test]
    fn narrowing_retains_the_issued_answer_set_and_exact_replay() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("db");
        let mut db = Database::init(&path)?;
        let id = existing(&db, None)?;
        assert!(crate::queue_store::seed_batch(&mut db)?);
        study_store::issue(&db, Some((&id, "tactic")))?;
        let view = study_store::current(&db)?.unwrap();
        let work = next(&db, 0)?.unwrap();
        assert_eq!(work.fen, FEN);
        let evidence = fixture_evidence(&work, B)?;
        db.execute("UPDATE cards SET title='Display only' WHERE id=?1", [&id])?;
        assert_eq!(submit(&mut db, &work, &evidence)?, "narrowed");
        assert_eq!(catalog::tactics(&db, "")?[0].route.moves, [B]);
        let answer = || AnswerIntentV1 {
            attempt: view.attempt,
            dataset: view.dataset.clone(),
            action: AnswerAction::Play(A.into()),
        };
        let accepted = serde_json::to_value(review::answer(&mut db, answer())?)?;
        assert_eq!(accepted["rating"], "Good");
        drop(db);
        let mut db = Database::init(&path)?;
        assert_eq!(
            serde_json::to_value(review::answer(&mut db, answer())?)?,
            accepted
        );
        assert_eq!(submit(&mut db, &work, &evidence)?, "narrowed");
        assert_eq!(
            db.query_row("SELECT answers FROM cards WHERE id=?1", [&id], |r| r
                .get::<_, String>(0))?,
            "[\"d1d5\"]"
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM learning WHERE card_id=?1",
                [&id],
                |r| r.get::<_, u32>(0)
            )?,
            1
        );
        Ok(())
    }

    #[test]
    fn retire_removes_due_and_committed_work_but_keeps_the_current_receipt() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut db = Database::init(&directory.path().join("db"))?;
        let id = existing(&db, Some(FEN))?;
        assert!(crate::queue_store::seed_batch(&mut db)?);
        crate::daily_store::prepare_test_day(&mut db, chrono::Utc::now().timestamp())?;
        study_store::issue(&db, Some((&id, "tactic")))?;
        let view = study_store::current(&db)?.unwrap();
        let answer = || AnswerIntentV1 {
            attempt: view.attempt,
            dataset: view.dataset.clone(),
            action: AnswerAction::Reveal,
        };
        let receipt = serde_json::to_value(review::answer(&mut db, answer())?)?;
        assert_eq!(receipt["rating"], "Again");
        let work = next(&db, 0)?.unwrap();
        let evidence = fixture_evidence(&work, "e1e2")?;
        assert_eq!(
            submit(&mut db, &work, &evidence)?,
            "best_outside_candidates"
        );
        assert!(catalog::tactics(&db, "")?.is_empty());
        assert!(!crate::queue_store::can_repeat(&db, &id)?);
        assert_eq!(db.query_row(
            "SELECT active,(SELECT count(*) FROM deck),(SELECT count(*) FROM repeat_obligation) FROM prepared_queue WHERE card_id=?1",
            [&id], |r| Ok((r.get::<_,u8>(0)?,r.get::<_,u8>(1)?,r.get::<_,u8>(2)?)))?, (0,0,0));
        assert_eq!(
            serde_json::to_value(review::answer(&mut db, answer())?)?,
            receipt
        );
        crate::daily_store::prepare_test_day(&mut db, chrono::Utc::now().timestamp())?;
        assert!(
            review::next(
                &mut db,
                Next {
                    after: Some(view.attempt),
                    dataset: Some(view.dataset)
                }
            )?
            .is_none()
        );
        assert_eq!(status(&db)?["removed"], 1);
        Ok(())
    }
}
