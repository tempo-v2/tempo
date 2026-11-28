use super::*;
use crate::{
    catalog::{OpeningInput, save_opening},
    db::Database,
    game_pipeline::seal_detector,
    games::{Acceptance, archive_game},
    review,
    study::{AnswerAction, AnswerIntentV1, Next},
};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    },
    time::Duration,
};

fn route(fen: &str, moves: &[&str]) -> chess::Route {
    chess::Route {
        fen: fen.into(),
        moves: moves.iter().map(|s| (*s).into()).collect(),
    }
}

fn opening(db: &Connection, name: &str, color: &str, moves: &[&str]) -> Result<String> {
    save_opening(
        db,
        &OpeningInput {
            name: name.into(),
            color: color.into(),
            route: route("", moves),
            start_at: 0,
        },
        None,
    )
}

fn game(db: &Connection, body: &str) -> Result<String> {
    let pgn =
        format!("[White \"Learner\"]\n[Black \"Other\"]\n[Result \"1/2-1/2\"]\n\n{body} 1/2-1/2");
    let archived = archive_game(chess::parse_pgn(&pgn, false, 1)?.remove(0)?, "Learner")?;
    match games::accept_game(db, &archived)? {
        Acceptance::Added(id) => Ok(id),
        _ => anyhow::bail!("game fixture already present"),
    }
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

fn get(db: &mut Connection, pair: &Pair) -> Result<View> {
    read(db, pair.clone(), ReadQuery::default())
}

fn start(db: &mut Connection, pair: &Pair) -> Result<i64> {
    let view = get(db, pair)?.status;
    let accepted = request(
        db,
        pair.clone(),
        Request {
            revision: view.revision,
            generation: view.generation,
        },
    )?;
    assert_eq!(accepted.state, State::Queued);
    Ok(work(db, pair)?.unwrap().job)
}

fn seal(db: &mut Connection, pair: &Pair, job: i64, file: &str) -> Result<Vec<u8>> {
    assert_eq!(job_store::next(db, None)?.unwrap().id, job);
    let bytes = prepare(db, pair.clone(), deadline())?;
    seal_detector(db, job, &pair.game, file)?;
    Ok(bytes)
}

fn make_batch(
    db: &mut Connection,
    job: i64,
    file: &str,
    before: usize,
    bytes: Vec<u8>,
) -> Result<Batch> {
    assert_eq!(job_store::next(db, None)?.unwrap().id, job);
    Batch::decode(job, claim(db, job)?, file.into(), before, bytes)
}

fn complete(db: &mut Connection, pair: &Pair) -> Result<View> {
    let job = start(db, pair)?;
    let bytes = seal(db, pair, job, "fixture")?;
    let batch = make_batch(db, job, "fixture", 0, bytes)?;
    assert!(batch.finished());
    apply(db, &batch)?;
    assert!(apply(db, &batch).is_err());
    get(db, pair)
}

#[test]
fn scores_occurrences_and_keeps_repertoires_separate() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut db = Database::init(&temp.path().join("scores"))?;
    let game_id = game(&db, "1. e4 e5 2. Nf3 Nc6 3. Bb5 a6")?;
    let book = opening(&db, "Italian", "white", &["e2e4", "e7e5", "f1c4"])?;
    let other = opening(&db, "Ruy", "white", &["e2e4", "e7e5", "g1f3"])?;
    let black = opening(&db, "Black", "black", &["e2e4", "e7e5"])?;
    let empty = opening(&db, "Other positions", "white", &["g1f3", "d7d5"])?;
    // Retiring the only reply leaves a retained choice but no authored answer.
    db.execute(
        "UPDATE cards SET opening_retired=1 WHERE repertoire_id=?1",
        [&empty],
    )?;
    let selection = choices(
        &mut db,
        game_id.clone(),
        ChoicesQuery {
            selected: Some(black.clone()),
            ..ChoicesQuery::default()
        },
    )?;
    assert_eq!(selection.choices.len(), 3);
    assert!(selection.selected.is_none());
    assert!(
        get(
            &mut db,
            &Pair {
                game: game_id.clone(),
                repertoire: black
            }
        )
        .is_err()
    );

    let pair = Pair {
        game: game_id.clone(),
        repertoire: book,
    };
    let view = complete(&mut db, &pair)?;
    assert!(view.status.current);
    let report = view.report.unwrap();
    assert_eq!(
        (report.followed, report.differed, report.unassessed),
        (1, 1, 1)
    );
    assert_eq!(report.decisions[1].played.san, "Nf3");
    assert!(matches!(&report.decisions[1].outcome,
        RepertoireOutcome::Differed { expected, .. } if expected.san == "Bc4"));
    assert!(matches!(
        report.decisions[2].outcome,
        RepertoireOutcome::Unassessed {
            reason: scanner::Unassessed::NoReply
        }
    ));

    let other = complete(
        &mut db,
        &Pair {
            game: game_id.clone(),
            repertoire: other,
        },
    )?
    .report
    .unwrap();
    assert_eq!(
        (other.followed, other.differed, other.unassessed),
        (2, 0, 1)
    );
    let empty = complete(
        &mut db,
        &Pair {
            game: game_id,
            repertoire: empty,
        },
    )?
    .report
    .unwrap();
    assert_eq!(
        (empty.followed, empty.differed, empty.unassessed),
        (0, 0, 3)
    );
    Ok(())
}

#[test]
fn revisits_repeated_positions_and_later_transpositions() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut db = Database::init(&temp.path().join("transposes"))?;
    let pair = Pair {
        game: game(&db, "1. Nf3 Nf6 2. Ng1 Ng8 3. Nf3 Nf6 4. Nc3 Nc6 5. g3")?,
        repertoire: opening(
            &db,
            "Transposition",
            "white",
            &["b1c3", "b8c6", "g1f3", "g8f6", "g2g3"],
        )?,
    };
    let report = complete(&mut db, &pair)?.report.unwrap();
    assert_eq!(
        (report.followed, report.differed, report.unassessed),
        (1, 2, 2)
    );
    assert_eq!(report.decisions[0].position, report.decisions[2].position);
    assert_eq!(report.decisions[0].ply, 0);
    assert_eq!(report.decisions[2].ply, 4);
    assert!(matches!(
        report.decisions[4].outcome,
        RepertoireOutcome::Followed { .. }
    ));
    assert_eq!(report.decisions[4].ply, 8);
    Ok(())
}

fn change_expected_move(db: &Connection, rep: &str, old: &str, new: &str) -> Result<()> {
    // An authoritative reply edit updates the same revision trigger used
    // by Openings' replacement. Keep the fixture on the storage boundary.
    let old_version = chess::id(&["answer-v1", old]);
    let changed = db.execute("UPDATE cards SET answers=?1,answer_version=?2 WHERE repertoire_id=?3 AND answer_version=?4",
        params![serde_json::to_string(&[new])?, chess::id(&["answer-v1", new]), rep, old_version],
    )?;
    assert_eq!(changed, 1);
    Ok(())
}

#[test]
fn snapshot_survives_edit_failed_replacement_and_recovery() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("restart");
    let mut db = Database::init(&path)?;
    let pair = Pair {
        game: game(&db, "1. e4 e5 2. Nf3 Nc6 3. Bb5 a6")?,
        repertoire: opening(&db, "Early", "white", &["d2d4", "d7d5", "c2c4"])?,
    };
    save_opening(
        &db,
        &OpeningInput {
            name: "Second position".into(),
            color: "white".into(),
            start_at: 2,
            route: route("", &["e2e4", "e7e5", "g1f3"]),
        },
        Some(&pair.repertoire),
    )?;
    let original = get(&mut db, &pair)?.status;
    let job = start(&mut db, &pair)?;
    let duplicate = request(
        &mut db,
        pair.clone(),
        Request {
            revision: original.revision,
            generation: original.generation,
        },
    )?;
    assert_eq!(duplicate.generation, Some(1));

    // The revision read establishes a WAL snapshot. Edit through a second
    // connection as the comparator starts its addressed reply lookups.
    let changed = Arc::new(AtomicBool::new(false));
    let triggered = changed.clone();
    let writer_path = path.clone();
    let repertoire = pair.repertoire.clone();
    db.flush_prepared_statement_cache();
    db.authorizer(Some(move |event: AuthContext<'_>| {
        if matches!(
            event.action,
            AuthAction::Read {
                table_name: "cards",
                ..
            }
        ) && !triggered.swap(true, Relaxed)
        {
            let writer = Database::open(&writer_path).unwrap();
            change_expected_move(&writer, &repertoire, "d2d4", "e2e4").unwrap();
            change_expected_move(&writer, &repertoire, "g1f3", "f1c4").unwrap();
        }
        Authorization::Allow
    }));
    let bytes = seal(&mut db, &pair, job, "original")?;
    db.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    assert!(changed.load(Relaxed));
    let stale_batch = make_batch(&mut db, job, "original", 0, bytes)?;
    apply(&mut db, &stale_batch)?;
    let old = get(&mut db, &pair)?;
    assert!(!old.status.current);
    let old_report = old.report.unwrap();
    assert_eq!(old_report.repertoire_revision, original.revision);
    assert_eq!((old_report.followed, old_report.differed), (1, 1));
    assert!(matches!(&old_report.decisions[0].outcome,
        RepertoireOutcome::Differed { expected, .. } if expected.san == "d4"));

    let retry_job = start(&mut db, &pair)?;
    assert_eq!(retry_job, job);
    let bytes = seal(&mut db, &pair, job, "replacement")?;
    let candidate = make_batch(&mut db, job, "replacement", 0, bytes.clone())?;
    assert!(apply(&mut db, &stale_batch).is_err());
    db.execute_batch(
        "CREATE TRIGGER fixture_write_failure BEFORE UPDATE OF report ON repertoire_feedback
                      BEGIN SELECT RAISE(ABORT,'failed replacement'); END;",
    )?;
    assert!(apply(&mut db, &candidate).is_err());
    assert!(
        job_store::next(
            &mut db,
            Some(&job_store::FailedTurn {
                job_id: job,
                message: "failed replacement".into(),
            })
        )?
        .is_none()
    );
    let failed = get(&mut db, &pair)?;
    assert_eq!(failed.status.state, State::Failed);
    assert_eq!(failed.status.completed.unwrap().generation, 1);
    assert_eq!(
        failed.report.unwrap().repertoire_revision,
        original.revision
    );
    db.execute_batch("DROP TRIGGER fixture_write_failure")?;

    assert_eq!(start(&mut db, &pair)?, job);
    assert_eq!(job_store::next(&mut db, None)?.unwrap().id, job);
    drop(db);
    let mut db = Database::init(&path)?;
    assert_eq!(job_store::recover_running(&mut db)?, 1);
    let retry = make_batch(&mut db, job, "replacement", 0, bytes)?;
    assert_eq!(retry.claim.generation, 3);
    apply(&mut db, &retry)?;
    let current = get(&mut db, &pair)?;
    assert!(current.status.current);
    let report = current.report.unwrap();
    assert_eq!((report.followed, report.differed), (1, 1));
    assert!(matches!(&report.decisions[1].outcome,
        RepertoireOutcome::Differed { expected, .. } if expected.san == "Bc4"));
    let writes = db.total_changes();
    assert!(
        request(
            &mut db,
            pair.clone(),
            Request {
                revision: current.status.revision,
                generation: current.status.generation,
            }
        )?
        .current
    );
    assert_eq!(db.total_changes(), writes);
    db.execute(
        "UPDATE cards SET title='Renamed' WHERE repertoire_id=?1",
        [&pair.repertoire],
    )?;
    assert!(get(&mut db, &pair)?.status.current);
    Ok(())
}

#[test]
fn conflicted_unusable_and_legacy_observations_have_no_denominator() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut db = Database::init(&temp.path().join("incomplete"))?;
    let pair = Pair {
        game: game(&db, "1. e4 e5 2. Nf3 Nc6 3. Bb5 a6")?,
        repertoire: opening(&db, "Conflicted", "white", &["e2e4", "e7e5", "g1f3"])?,
    };
    db.execute(
        "UPDATE cards SET quarantined=1 WHERE repertoire_id=?1 AND answer_version=?2",
        params![pair.repertoire, chess::id(&["answer-v1", "e2e4"])],
    )?;
    db.execute(
        "UPDATE cards SET answers='[\"h1h8\"]' WHERE repertoire_id=?1 AND answer_version=?2",
        params![pair.repertoire, chess::id(&["answer-v1", "g1f3"])],
    )?;
    db.execute(
        "INSERT INTO mismatches VALUES (?1,0,'historical',?2,'answer','e2e4',1)",
        (&pair.game, &pair.repertoire),
    )?;
    let legacy = get(&mut db, &pair)?;
    assert!(legacy.status.legacy_only && legacy.report.is_none());
    let report = complete(&mut db, &pair)?.report.unwrap();
    assert_eq!(
        (report.followed, report.differed, report.unassessed),
        (0, 0, 3)
    );
    assert!(matches!(
        report.decisions[0].outcome,
        RepertoireOutcome::Unassessed {
            reason: scanner::Unassessed::Conflict
        }
    ));
    assert!(matches!(
        report.decisions[1].outcome,
        RepertoireOutcome::Unassessed {
            reason: scanner::Unassessed::Unusable
        }
    ));
    Ok(())
}

#[test]
fn report_work_is_addressed_and_study_does_not_read_it() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("bounded");
    let mut db = Database::init(&path)?;
    let pair = Pair {
        game: game(&db, "1. e4 e5 2. Nf3 Nc6")?,
        repertoire: opening(&db, "Book", "white", &["e2e4", "e7e5", "f1c4"])?,
    };
    complete(&mut db, &pair)?;
    let measure = |db: &mut Connection| -> Result<usize> {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        db.progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, Relaxed);
                false
            }),
        );
        assert!(get(db, &pair)?.status.current);
        db.progress_handler(0, None::<fn() -> bool>);
        Ok(count.load(Relaxed))
    };
    let small = measure(&mut db)?;
    db.execute(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<5000)
         INSERT INTO games(id,white,black,learner_color,game_ir,occurrence,outcome)
         SELECT printf('extra-%05d',i),white,black,learner_color,game_ir,occurrence,outcome
         FROM games CROSS JOIN n WHERE id=?1",
        [&pair.game],
    )?;
    let large = measure(&mut db)?;
    assert!(
        large <= small + 12,
        "report VM steps grew from {small} to {large}"
    );

    change_expected_move(&db, &pair.repertoire, "f1c4", "g1f3")?;
    let job = start(&mut db, &pair)?;
    while !crate::queue_store::seed_batch(&mut db)? {}
    crate::daily_store::prepare_test_day(&mut db, chrono::Utc::now().timestamp())?;
    db.flush_prepared_statement_cache();
    db.authorizer(Some(|event: AuthContext<'_>| match event.action {
        AuthAction::Read {
            table_name: "games" | "jobs" | "repertoire_feedback" | "mismatches",
            ..
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }));
    let first = review::next(
        &mut db,
        Next {
            dataset: None,
            after: None,
        },
    )?
    .unwrap();
    let first_feedback = review::answer(
        &mut db,
        AnswerIntentV1 {
            attempt: first.attempt,
            dataset: first.dataset.clone(),
            action: AnswerAction::Reveal,
        },
    )?;
    assert_eq!(serde_json::to_value(first_feedback)?["rating"], "Again");
    db.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    db.execute(
        "UPDATE jobs SET status='failed',error='fixture' WHERE id=?1",
        [job],
    )?;
    db.flush_prepared_statement_cache();
    db.authorizer(Some(|event: AuthContext<'_>| match event.action {
        AuthAction::Read {
            table_name: "games" | "jobs" | "repertoire_feedback" | "mismatches",
            ..
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }));
    let second = review::next(
        &mut db,
        Next {
            dataset: Some(first.dataset),
            after: Some(first.attempt),
        },
    )?
    .unwrap();
    let second_feedback = review::answer(
        &mut db,
        AnswerIntentV1 {
            attempt: second.attempt,
            dataset: second.dataset,
            action: AnswerAction::Reveal,
        },
    )?;
    assert_eq!(serde_json::to_value(second_feedback)?["rating"], "Again");
    Ok(())
}

#[test]
fn bounded_apply_restarts_without_publishing_a_partial_report() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("batches");
    let mut db = Database::init(&path)?;
    let movetext = (1..=52)
        .map(|turn| {
            format!(
                "{turn}. {}",
                if turn % 2 == 1 { "Nf3 Nf6" } else { "Ng1 Ng8" }
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let pair = Pair {
        game: game(&db, &movetext)?,
        repertoire: opening(&db, "Repetition", "white", &["d2d4"])?,
    };
    let job = start(&mut db, &pair)?;
    let bytes = seal(&mut db, &pair, job, "bounded")?;
    let batch = make_batch(&mut db, job, "bounded", 0, bytes.clone())?;
    assert!(!batch.finished());
    apply(&mut db, &batch)?;
    assert!(get(&mut db, &pair)?.report.is_none());
    drop(db);

    let mut db = Database::init(&path)?;
    assert_eq!(
        db.query_row("SELECT window_offset FROM jobs WHERE id=?1", [job], |row| {
            row.get::<_, usize>(0)
        })?,
        MAX_FINDINGS_PER_APPLY
    );
    let last = make_batch(&mut db, job, "bounded", MAX_FINDINGS_PER_APPLY, bytes)?;
    assert!(apply(&mut db, &batch).is_err());
    assert!(last.finished());
    apply(&mut db, &last)?;
    assert!(apply(&mut db, &last).is_err());
    let finished = get(&mut db, &pair)?;
    let report = finished.report.unwrap();
    assert_eq!(
        (report.followed, report.differed, report.unassessed),
        (0, 26, 26)
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM mismatches", [], |row| row
            .get::<_, usize>(0))?,
        26
    );
    Ok(())
}

#[test]
fn coalesces_before_admission_and_keeps_import_capacity_separate() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut db = Database::init(&temp.path().join("capacity"))?;
    let game = game(&db, "1. e4 e5")?;
    let mut pairs = Vec::new();
    for index in 0..5 {
        pairs.push(Pair {
            game: game.clone(),
            repertoire: opening(&db, &format!("Book {index}"), "white", &["e2e4"])?,
        });
    }
    for pair in &pairs[..4] {
        start(&mut db, pair)?;
    }
    let joined = request(
        &mut db,
        pairs[0].clone(),
        Request {
            revision: 1,
            generation: Some(1),
        },
    )?;
    assert_eq!(joined.generation, Some(1));
    let new = get(&mut db, &pairs[4])?.status;
    let error = request(
        &mut db,
        pairs[4].clone(),
        Request {
            revision: new.revision,
            generation: new.generation,
        },
    )
    .err()
    .unwrap();
    assert!(error.is::<QueueFull>());
    assert!(work(&db, &pairs[4])?.is_none());
    job_store::start(
        &mut db,
        crate::jobs::NewJob::Command(crate::jobs::StartJob {
            provider: "archive".into(),
            operation: "scan".into(),
        }),
    )?;
    Ok(())
}
