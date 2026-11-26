// Coordinate the accepted answer across Study, Chess, Learning and Queue.
use crate::{
    chess, db, full_line, full_line_store,
    learning::{self, Rating},
    queue, queue_store,
    study::{
        AcceptedView, AnswerAction, AnswerIntentV1, AnswerState, Conflict, Next, NextState,
        RepeatDisposition, Review,
    },
    study_store,
};
use anyhow::Result;
use chrono::Utc;
use rusqlite::{Connection, TransactionBehavior::Immediate};

pub fn counts(db: &mut Connection) -> Result<Option<queue::DisplayCounts>> {
    let snapshot = db.transaction()?;
    let unanswered = study_store::unanswered_card(&snapshot)?;
    Ok(
        queue_store::counts(&snapshot, Utc::now().timestamp(), unanswered.as_deref())?
            .map(queue::DisplayCounts::from),
    )
}

pub fn answer(db: &mut Connection, input: AnswerIntentV1) -> Result<AcceptedView> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let issued = match study_store::begin_answer(&tx, &input)? {
        AnswerState::Replayed(view) => return Ok(view),
        AnswerState::Pending(issued) => issued,
    };
    let rating = if chess::answer_matches(&issued.answers, issued.action.played()) {
        Rating::Good
    } else {
        Rating::Again
    };
    let now = Utc::now().timestamp();
    let (repeat, _) = record_rating(
        &tx,
        &issued.card,
        input.attempt,
        issued.previous_kind.as_deref(),
        rating,
        now,
        None,
    )?;
    let accepted = study_store::accept_answer(&tx, &input, issued, rating, repeat)?;
    tx.commit()?;
    Ok(accepted)
}

pub enum FullLineSurface {
    Study,
    Tactics {
        first_card: fn(&str) -> full_line::InitialCard,
    },
}

pub fn move_full_line(
    db: &mut Connection,
    input: full_line::Move,
    surface: FullLineSurface,
) -> Result<full_line::MoveReply> {
    let tx = db.transaction_with_behavior(Immediate)?;
    full_line_store::check_dataset(&tx, &input.dataset)?;
    full_line_store::check_surface(
        &tx,
        input.play,
        match surface {
            FullLineSurface::Study => full_line::Surface::Study,
            FullLineSurface::Tactics { .. } => full_line::Surface::Tactics,
        },
    )?;
    let action = input
        .played
        .clone()
        .map_or(AnswerAction::Reveal, AnswerAction::Play);
    let encoded = serde_json::to_string(&action)?;
    if let Some(reply) = full_line_store::replay_move(&tx, input.play, input.step, &encoded)? {
        return Ok(reply);
    }

    let issued = full_line_store::load(&tx, input.play)?;
    anyhow::ensure!(
        issued.result.is_none() && issued.ply as i64 == input.step,
        Conflict::Attempt
    );
    if matches!(surface, FullLineSurface::Study) {
        let (active, _) = study_store::state(&tx)?;
        anyhow::ensure!(issued.review == active, Conflict::Attempt);
    }
    let before_fen = chess::fen(&chess::board_at(&issued.route, issued.ply)?);
    let step = full_line::advance(&issued.route, issued.ply, &action)?;
    let mut reviewed = None;
    let result = if let Some(rating) = step.rating {
        let mut outcome = full_line::Outcome {
            rating,
            repeat: RepeatDisposition::None,
            due_day: None,
        };
        if issued.mode != "practice" {
            let first = issued.mode == "first";
            let (card, attempt) = if first {
                let FullLineSurface::Tactics { first_card } = surface else {
                    return Err(Conflict::Attempt.into());
                };
                full_line_store::admit(&tx, input.play, &issued, first_card(&issued.puzzle))?
            } else {
                let attempt = issued.review.ok_or(Conflict::Attempt)?;
                (full_line_store::reviewed_card(&tx, attempt)?, attempt)
            };
            let now = Utc::now().timestamp();
            let first_due = if first && matches!(rating, Rating::Good) {
                Some(learning::first_pack_due(now, learning::draw_pack_days())?)
            } else {
                None
            };
            let previous = if matches!(rating, Rating::Again) {
                study_store::state(&tx)?.1
            } else {
                None
            };
            (outcome.repeat, outcome.due_day) = record_rating(
                &tx,
                &card,
                attempt,
                previous.as_deref(),
                rating,
                now,
                first_due,
            )?;
            reviewed = Some(attempt);
        }
        Some(outcome)
    } else {
        None
    };
    let view = full_line_store::commit_move(&tx, &input, &encoded, step, result, reviewed)?;
    tx.commit()?;
    Ok(full_line::MoveReply { view, before_fen })
}

/// Apply an authoritative assessment from either first play or Study.
pub fn record_rating(
    db: &Connection,
    card: &str,
    attempt: i64,
    previous: Option<&str>,
    rating: Rating,
    now: i64,
    first_due: Option<(i64, String)>,
) -> Result<(RepeatDisposition, Option<String>)> {
    let prior = db::prior_review(db, card)?;
    anyhow::ensure!(
        first_due.is_none() || prior.is_none(),
        "First review already exists"
    );
    let mut schedule = learning::schedule(rating, prior, now)?;
    let day = first_due.map(|(due, day)| {
        schedule.due_at = due;
        day
    });
    db::save_review(db, card, now, schedule)?;
    let repeat = if matches!(rating, Rating::Again) && queue_store::can_repeat(db, card)? {
        let draw = queue::draw_repeat_offset();
        if queue_store::can_place_now(db)? {
            RepeatDisposition::Placed(queue_store::place_repeat(db, card, previous, now, draw)?)
        } else {
            queue_store::defer_repeat(db, attempt, card, previous, now, draw)?;
            RepeatDisposition::Deferred
        }
    } else {
        RepeatDisposition::None
    };
    Ok((repeat, day))
}

#[derive(Debug)]
pub struct Preparing;
impl std::fmt::Display for Preparing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Preparing the Study queue. Retry Next.")
    }
}
impl std::error::Error for Preparing {}

pub fn next(db: &mut Connection, input: Next) -> Result<Option<Review>> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let NextState::Select { previous_kind } = study_store::begin_next(&tx, &input)? else {
        return study_store::current(&tx);
    };
    anyhow::ensure!(queue_store::prepared(&tx)?, Preparing);
    queue_store::place_deferred(&tx)?;
    let selected = match queue_store::committed_head(&tx)? {
        Some(card) => Some(card),
        None => queue_store::select(&tx, previous_kind.as_deref(), Utc::now().timestamp())?,
    };
    study_store::issue(
        &tx,
        selected
            .as_ref()
            .map(|(id, kind)| (id.as_str(), kind.as_str())),
    )?;
    if let Some((id, _)) = selected {
        queue_store::consume(&tx, &id)?;
    }
    let view = study_store::current(&tx)?;
    tx.commit()?;
    Ok(view)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_line_and_ordinary_publication_keep_distinct_cards_in_both_orders() -> Result<()> {
        use crate::catalog::{self, TacticInput};
        use rusqlite::params;

        for full_line_first in [false, true] {
            let files = tempfile::tempdir()?;
            let mut db = db::Database::init(&files.path().join("cards.sqlite3"))?;
            let route = chess::Route {
                fen: chess::fen(&chess::position("")?),
                moves: vec!["e2e4".into(), "e7e5".into(), "g1f3".into()],
            };
            let exercise = |version: &str, route: chess::Route| {
                full_line::PublishedExercise::from_source(
                    "publisher",
                    "shared",
                    version.into(),
                    route,
                )
            };
            let first = exercise("1", route.clone())?;
            let register_full_line = |db: &mut Connection| -> Result<String> {
                let tx = db.transaction_with_behavior(Immediate)?;
                full_line_store::publish(&tx, &first)?;
                let id = full_line_store::register_card(&tx, &first, "First title")?;
                tx.commit()?;
                Ok(id)
            };
            let early_card = if full_line_first {
                Some(register_full_line(&mut db)?)
            } else {
                None
            };
            let ordinary = catalog::apply_tactic(
                &db,
                &catalog::prepare_tactic(TacticInput {
                    name: "Single-answer card",
                    route: &route,
                    answers: &route.moves[..1],
                    source: None,
                })?,
            )?;
            while !queue_store::seed_batch(&mut db)? {}
            db.execute("INSERT INTO deck(slot,card_id) VALUES (1,?1)", [&ordinary])?;
            let issued = next(
                &mut db,
                Next {
                    after: None,
                    dataset: None,
                },
            )?
            .unwrap();
            assert!(matches!(
                issued.activity,
                crate::study::Activity::SingleAnswer { .. }
            ));
            let intent = || AnswerIntentV1 {
                attempt: issued.attempt,
                dataset: issued.dataset.clone(),
                action: AnswerAction::Play("e2e4".into()),
            };
            let accepted = serde_json::to_value(answer(&mut db, intent())?)?;
            let card = match early_card {
                Some(id) => id,
                None => register_full_line(&mut db)?,
            };
            assert_ne!(card, ordinary);

            let tx = db.transaction_with_behavior(Immediate)?;
            let updated = exercise("2", route.clone())?;
            full_line_store::publish(&tx, &updated)?;
            let changes = tx.total_changes();
            assert_eq!(
                full_line_store::register_card(&tx, &first, "Replay title")?,
                card
            );
            assert!(full_line_store::register_card(&tx, &updated, "New revision").is_err());
            let conflicting_route = exercise(
                "1",
                chess::Route {
                    fen: route.fen.clone(),
                    moves: vec!["a2a3".into()],
                },
            )?;
            assert!(
                full_line_store::register_card(&tx, &conflicting_route, "Different content")
                    .is_err()
            );
            let legacy =
                full_line::PublishedExercise::from_legacy_pack("00000".into(), "1".into(), route)?;
            assert!(full_line_store::register_card(&tx, &legacy, "Bypass first play").is_err());
            assert_eq!(tx.total_changes(), changes);
            let title: String =
                tx.query_row("SELECT title FROM cards WHERE id=?1", [&card], |r| r.get(0))?;
            assert_eq!(title, "First title");
            tx.commit()?;

            assert!(matches!(
                study_store::current(&db)?.unwrap().activity,
                crate::study::Activity::SingleAnswer { result: Some(_) }
            ));
            let changes = db.total_changes();
            assert_eq!(serde_json::to_value(answer(&mut db, intent())?)?, accepted);
            assert_eq!(db.total_changes(), changes);

            db.execute(
                "INSERT INTO deck(slot,card_id) VALUES (1,?1)",
                params![card],
            )?;
            let full = next(
                &mut db,
                Next {
                    after: Some(issued.attempt),
                    dataset: Some(issued.dataset.clone()),
                },
            )?
            .unwrap();
            let crate::study::Activity::FullLine { puzzle, .. } = full.activity else {
                panic!("Independent card must issue as a full line");
            };
            move_full_line(
                &mut db,
                full_line::Move {
                    dataset: full.dataset,
                    play: puzzle.play,
                    step: 0,
                    played: None,
                },
                FullLineSurface::Study,
            )?;
            assert_eq!(
                db.query_row("SELECT count(*) FROM learning", [], |r| r.get::<_, i64>(0))?,
                2
            );
        }
        Ok(())
    }

    #[test]
    fn issued_full_line_from_another_source_survives_catalog_failure() -> Result<()> {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

        for (first_move, solves) in [("e2e4", true), ("d2d4", false)] {
            let directory = tempfile::tempdir()?;
            let mut db = db::Database::init(&directory.path().join("study.sqlite3"))?;
            let route = chess::Route {
                fen: chess::fen(&chess::position("")?),
                moves: if solves {
                    vec![first_move.into(), "e7e5".into(), "g1f3".into()]
                } else {
                    vec![first_move.into(), "d7d5".into(), "c2c4".into()]
                },
            };
            let tx = db.transaction_with_behavior(Immediate)?;
            let published = |route| {
                full_line::PublishedExercise::from_source("fixture", "exercise", "1".into(), route)
            };
            let invalid = chess::Route {
                fen: route.fen.clone(),
                moves: vec!["a2a5".into()],
            };
            assert!(published(invalid).is_err());
            let castle = |played: &str| chess::Route {
                fen: "4k3/8/8/8/8/8/8/4K2R w K - 0 1".into(),
                moves: vec![played.into()],
            };
            assert!(published(castle("e1h1")).is_err());
            assert!(published(castle("e1g1")).is_ok());

            let exercise = published(chess::Route {
                fen: String::new(),
                moves: route.moves.clone(),
            })?;
            assert_eq!(exercise.route().fen, route.fen);
            full_line_store::publish(&tx, &exercise)?;
            full_line_store::publish(&tx, &exercise)?;
            let other_route = chess::Route {
                fen: route.fen.clone(),
                moves: vec!["a2a3".into()],
            };
            assert!(full_line_store::publish(&tx, &published(other_route)?).is_err());
            full_line_store::register_card(&tx, &exercise, "Imported exercise")?;
            tx.commit()?;
            while !queue_store::seed_batch(&mut db)? {}
            db.flush_prepared_statement_cache();
            db.authorizer(Some(|context: AuthContext<'_>| match context.action {
                AuthAction::Read {
                    table_name: "lichess_catalog" | "lichess_packs" | "lichess_members",
                    ..
                } => Authorization::Deny,
                _ => Authorization::Allow,
            }));

            let issued = next(
                &mut db,
                Next {
                    after: None,
                    dataset: None,
                },
            )?
            .unwrap();
            let crate::study::Activity::FullLine { puzzle, .. } = issued.activity else {
                panic!("Published version must issue as a full line");
            };
            assert_eq!(puzzle.puzzle, "fixture:exercise");
            assert_eq!(puzzle.fen, route.fen);
            assert!(puzzle.solution.is_none());
            assert_eq!(
                serde_json::to_value(study_store::current(&db)?.unwrap().activity)?["puzzle"],
                serde_json::to_value(&puzzle)?,
            );
            assert!(matches!(
                answer(
                    &mut db,
                    AnswerIntentV1 {
                        attempt: issued.attempt,
                        dataset: issued.dataset.clone(),
                        action: AnswerAction::Reveal,
                    }
                )
                .err()
                .and_then(|e| e.downcast::<Conflict>().ok()),
                Some(Conflict::FullSolution)
            ));

            let movement = |step, played: Option<&str>| full_line::Move {
                dataset: issued.dataset.clone(),
                play: puzzle.play,
                step,
                played: played.map(str::to_owned),
            };
            let first = move_full_line(&mut db, movement(0, Some("e2e4")), FullLineSurface::Study)?;
            let finish = if solves {
                assert_eq!(first.view.step, 2);
                assert!(first.view.result.is_none());
                let learned: i64 =
                    db.query_row("SELECT count(*) FROM learning", [], |r| r.get(0))?;
                assert_eq!(learned, 0);
                move_full_line(&mut db, movement(2, None), FullLineSurface::Study)?
            } else {
                assert_eq!(first.view.step, 0);
                move_full_line(&mut db, movement(0, Some("e2e4")), FullLineSurface::Study)?
            };
            assert!(matches!(
                finish.view.result.as_ref().unwrap().rating,
                Rating::Again
            ));
            assert!(matches!(
                finish.view.result.as_ref().unwrap().repeat,
                RepeatDisposition::Placed(_)
            ));
            let next_issued = next(
                &mut db,
                Next {
                    after: Some(issued.attempt),
                    dataset: Some(issued.dataset.clone()),
                },
            )?
            .unwrap();
            assert_ne!(next_issued.attempt, issued.attempt);
            let changes = db.total_changes();
            let replay =
                move_full_line(&mut db, movement(0, Some("e2e4")), FullLineSurface::Study)?;
            assert_eq!(serde_json::to_value(replay)?, serde_json::to_value(first)?);
            assert_eq!(db.total_changes(), changes);
            assert_eq!(
                db.query_row("SELECT count(*) FROM learning", [], |r| r.get::<_, i64>(0))?,
                1
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM lichess_moves", [], |r| r
                    .get::<_, i64>(0))?,
                if solves { 2 } else { 1 }
            );
        }
        Ok(())
    }

    #[test]
    fn committed_next_and_again_work_stays_bounded_as_catalog_grows() -> Result<()> {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::Relaxed},
        };

        fn measure(noise: usize, first: &str) -> Result<(String, usize, usize)> {
            let mut db = Connection::open_in_memory()?;
            for schema in [
                include_str!("schema.sql"),
                include_str!("opening_policy.sql"),
                include_str!("opening_order.sql"),
                include_str!("game_pipeline.sql"),
                include_str!("prepared_queue.sql"),
                include_str!("card_availability.sql"),
                include_str!("lichess_packs.sql"),
                include_str!("coverage_order.sql"),
            ] {
                db.execute_batch(schema)?;
            }
            db.execute_batch("INSERT INTO metadata VALUES ('fixture')")?;
            let fen = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
            for id in ["a", "b"] {
                db.execute(
                    r#"INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
                     VALUES (?1,'tactic',?1,'white',?2,?1,'["e2e4"]')"#,
                    (id, fen),
                )?;
            }
            db.execute_batch(
                "INSERT INTO cards(id,kind,title,color,fen,position_id,answers,quarantined)
                 VALUES ('quarantined','opening','','white','unused','unused','[]',1);
                 INSERT INTO deck VALUES (1,'quarantined');
                 UPDATE prepared_queue_state SET ready=1;",
            )?;
            db.execute(
                "INSERT INTO deck VALUES (2,?1),(3,?2)",
                (first, if first == "a" { "b" } else { "a" }),
            )?;
            db.execute(
                "WITH RECURSIVE n(i) AS (SELECT 1 WHERE ?1>0 UNION ALL SELECT i+1 FROM n WHERE i<?1)
                 INSERT INTO cards(id,kind,title,color,fen,position_id,answers,quarantined)
                 SELECT printf('unrelated%06d',i),'opening','','white','unused','unused','[]',1 FROM n",
                [noise],
            )?;
            let steps = Arc::new(AtomicUsize::new(0));
            let count = steps.clone();
            db.progress_handler(
                1,
                Some(move || {
                    count.fetch_add(1, Relaxed);
                    false
                }),
            );
            let review = next(
                &mut db,
                Next {
                    after: None,
                    dataset: None,
                },
            )?
            .unwrap();
            let next_steps = steps.swap(0, Relaxed);
            answer(
                &mut db,
                AnswerIntentV1 {
                    attempt: review.attempt,
                    dataset: review.dataset,
                    action: crate::study::AnswerAction::Reveal,
                },
            )?;
            Ok((review.title, next_steps, steps.load(Relaxed)))
        }

        let small = measure(0, "a")?;
        let large = measure(100_000, "a")?;
        assert_eq!(small.0, "a");
        assert_eq!(large.0, "a");
        assert!(
            large.1 <= small.1 + 50,
            "Next steps grew from {} to {}",
            small.1,
            large.1
        );
        assert!(
            large.2 <= small.2 + 50,
            "Again steps grew from {} to {}",
            small.2,
            large.2
        );
        assert_eq!(measure(0, "b")?.0, "b");
        Ok(())
    }

    #[test]
    fn studied_commands_do_not_read_games_or_live_rank() -> Result<()> {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        let mut db = Connection::open_in_memory()?;
        for schema in [
            include_str!("schema.sql"),
            include_str!("opening_policy.sql"),
            include_str!("opening_order.sql"),
            include_str!("game_pipeline.sql"),
            include_str!("prepared_queue.sql"),
            include_str!("card_availability.sql"),
            include_str!("lichess_packs.sql"),
            include_str!("coverage_order.sql"),
        ] {
            db.execute_batch(schema)?;
        }
        db.execute("INSERT INTO metadata VALUES ('fixture')", [])?;
        db.execute_batch(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<2500)
             INSERT INTO cards(id,kind,title,color,fen,position_id,answers,quarantined)
             SELECT printf('unavailable%04d',i),'opening','', 'white','unused','unused','[]',1 FROM n",
        )?;
        db.execute(
            "INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
            VALUES ('a','tactic','Title','white',?1,'a','[\"e2e4\"]')",
            ["rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1"],
        )?;
        while !queue_store::seed_batch(&mut db)? {}
        db.flush_prepared_statement_cache();
        db.authorizer(Some(|context: AuthContext<'_>| match context.action {
            AuthAction::Read {
                table_name:
                    "games"
                    | "jobs"
                    | "mismatches"
                    | "missed_captures"
                    | "capture_qualification"
                    | "opening_order",
                ..
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }));
        let review = next(
            &mut db,
            Next {
                after: None,
                dataset: None,
            },
        )?
        .unwrap();
        assert_eq!(review.title, "Title");
        let accepted = answer(
            &mut db,
            AnswerIntentV1 {
                attempt: review.attempt,
                dataset: review.dataset,
                action: crate::study::AnswerAction::Play("e2e4".into()),
            },
        )?;
        assert_eq!(serde_json::to_value(accepted)?["rating"], "Good");
        Ok(())
    }

    #[test]
    fn accepted_again_survives_an_unprepared_queue_and_places_once() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        for schema in [
            include_str!("schema.sql"),
            include_str!("opening_policy.sql"),
            include_str!("opening_order.sql"),
            include_str!("game_pipeline.sql"),
            include_str!("prepared_queue.sql"),
            include_str!("card_availability.sql"),
            include_str!("lichess_packs.sql"),
            include_str!("coverage_order.sql"),
        ] {
            db.execute_batch(schema)?;
        }
        db.execute("INSERT INTO metadata VALUES ('fixture')", [])?;
        db.execute(
            "INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
             VALUES ('a','tactic','First','white',?1,'a','[\"e2e4\"]')",
            ["rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1"],
        )?;
        study_store::issue(&db, Some(("a", "tactic")))?;
        let review = next(
            &mut db,
            Next {
                after: None,
                dataset: None,
            },
        )?
        .unwrap();
        let answer_command = || AnswerIntentV1 {
            attempt: review.attempt,
            dataset: "fixture".into(),
            action: crate::study::AnswerAction::Reveal,
        };
        let first = serde_json::to_value(answer(&mut db, answer_command())?)?;
        assert_eq!(first["rating"], "Again");
        let draw: u8 = db.query_row("SELECT draw FROM repeat_obligation", [], |row| row.get(0))?;
        let continuation = || Next {
            after: Some(review.attempt),
            dataset: Some("fixture".into()),
        };
        assert!(matches!(next(&mut db, continuation()), Err(error) if error.is::<Preparing>()));
        assert_eq!(
            serde_json::to_value(answer(&mut db, answer_command())?)?,
            first
        );
        assert_eq!(
            db.query_row("SELECT draw FROM repeat_obligation", [], |row| row
                .get::<_, u8>(0))?,
            draw
        );
        assert!(queue_store::seed_batch(&mut db)?);
        let next_attempt = next(&mut db, continuation())?.unwrap().attempt;
        assert_ne!(next_attempt, review.attempt);
        assert_eq!(
            next(&mut db, continuation())?.unwrap().attempt,
            next_attempt
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM repeat_obligation", [], |row| row
                .get::<_, u32>(0))?,
            0
        );
        assert_eq!(
            serde_json::to_value(answer(&mut db, answer_command())?)?,
            first
        );
        Ok(())
    }
}
