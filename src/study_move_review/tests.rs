use super::*;
use crate::{
    learning::Rating,
    review,
    study::{AnswerAction, AnswerIntentV1},
};

fn opening(name: &str, moves: &str, start_at: usize) -> catalog::OpeningInput {
    catalog::OpeningInput {
        name: name.into(),
        color: "white".into(),
        route: chess::Route {
            fen: String::new(),
            moves: moves.split_whitespace().map(str::to_owned).collect(),
        },
        start_at,
    }
}

fn position(line: &catalog::OpeningInput, ply: usize) -> Result<String> {
    Ok(chess::position_id(&chess::board_at(&line.route, ply)?))
}

fn issue(db: &Connection, card: &str) -> Result<Address> {
    study_store::issue(db, Some((card, "opening")))?;
    Ok(Address {
        dataset: crate::db::dataset(db)?,
        attempt: db.last_insert_rowid(),
    })
}

fn answer(db: &mut Connection, address: &Address) -> Result<()> {
    review::answer(
        db,
        AnswerIntentV1 {
            dataset: address.dataset.clone(),
            attempt: address.attempt,
            action: AnswerAction::Reveal,
        },
    )?;
    Ok(())
}

#[test]
fn removal_retires_only_unsupported_versions_and_replays_after_next() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut db = Database::init(&temp.path().join("study.sqlite3"))?;
    let main = opening("Main", "g1f3 d7d5 g2g3 g8f6 f1g2", 0);
    let transposed = opening("Transposition", "g2g3 g8f6 g1f3 d7d5 f1g2", 4);
    let repertoire = catalog::save_opening(&db, &main, None)?;
    catalog::save_opening(&db, &transposed, Some(&repertoire))?;
    let root = position(&main, 0)?;
    let removed = position(&main, 2)?;
    let retained = position(&main, 4)?;
    assert_eq!(position(&transposed, 4)?, retained);
    let saved = catalog::current_opening_reply(&db, &repertoire, &removed)?.unwrap();
    let survivor = catalog::current_opening_reply(&db, &repertoire, &retained)?.unwrap();
    let address = issue(&db, &saved.id)?;
    assert!(matches!(current(&db, &address), Err(error) if error.is::<Stale>()));
    answer(&mut db, &address)?;
    let (basis, _) = current(&db, &address)?;
    let edit = Edit {
        basis,
        action: Action::Remove,
    };
    let replacement = plan(&db, &edit)?;
    assert_eq!(replacement.impact.len(), 1);
    assert!(!replacement.over_budget);
    let command = edit.command()?;
    let receipt = commit(&mut db, &edit, &command, &replacement)?;
    assert!(catalog::current_opening_reply(&db, &repertoire, &removed)?.is_none());
    assert_eq!(
        catalog::current_opening_reply(&db, &repertoire, &retained)?
            .unwrap()
            .id,
        survivor.id
    );
    let storage: (String, f64) = db.query_row(
        "SELECT a.result,l.stability FROM attempts a JOIN learning l ON l.card_id=a.card_id WHERE a.id=?1",
        [address.attempt], |row| Ok((row.get(0)?,row.get(1)?)))?;
    let result: crate::study::AnswerReceipt = serde_json::from_str(&storage.0)?;
    assert_eq!(result.answers(), ["g2g3"]);
    assert!(
        matches!(result, crate::study::AnswerReceipt::Versioned { v1 } if matches!(v1.rating, Rating::Again))
    );
    issue(
        &db,
        &catalog::current_opening_reply(&db, &repertoire, &root)?
            .unwrap()
            .id,
    )?;
    let before = db.total_changes();
    assert_eq!(commit(&mut db, &edit, &command, &replacement)?, receipt);
    assert_eq!(db.total_changes(), before);
    Ok(())
}

#[test]
fn replacement_fences_a_new_attempt_and_a_line_only_change() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut db = Database::init(&temp.path().join("study.sqlite3"))?;
    let main = opening("Main", "e2e4 e7e5 g1f3", 0);
    let repertoire = catalog::save_opening(&db, &main, None)?;
    let root = position(&main, 0)?;
    let saved = catalog::current_opening_reply(&db, &repertoire, &root)?.unwrap();
    let address = issue(&db, &saved.id)?;
    answer(&mut db, &address)?;
    let replacement = |db: &Connection, address: &Address| -> Result<_> {
        let (basis, _) = current(db, address)?;
        let edit = Edit {
            basis,
            action: Action::Replace {
                movement: "d2d4".into(),
            },
        };
        let plan = plan(db, &edit)?;
        Ok((edit, plan))
    };
    let (old, old_plan) = replacement(&db, &address)?;
    catalog::save_opening(
        &db,
        &opening("Same first reply", "e2e4", 0),
        Some(&repertoire),
    )?;
    let current_basis = current(&db, &address)?.0;
    assert_eq!(current_basis.reply_revision, old.basis.reply_revision);
    assert_ne!(current_basis.line_revision, old.basis.line_revision);
    assert!(
        commit(&mut db, &old, &old.command()?, &old_plan)
            .unwrap_err()
            .is::<Stale>()
    );
    let (edit, plan) = replacement(&db, &address)?;
    let next = issue(&db, &saved.id)?;
    assert!(
        commit(&mut db, &edit, &edit.command()?, &plan)
            .unwrap_err()
            .is::<Stale>()
    );
    answer(&mut db, &next)?;
    let (edit, plan) = replacement(&db, &next)?;
    assert_eq!(plan.impact.len(), 2);
    commit(&mut db, &edit, &edit.command()?, &plan)?;
    let new = catalog::current_opening_reply(&db, &repertoire, &root)?.unwrap();
    assert_ne!(new.version, saved.version);
    assert_eq!(new.answers, r#"["d2d4"]"#);
    let learned: i64 = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM learning WHERE card_id=?1)",
        [&new.id],
        |r| r.get(0),
    )?;
    assert_eq!(learned, 0);
    Ok(())
}

#[test]
fn oversized_removal_leaves_every_route_and_receipt_unmodified() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut db = Database::init(&temp.path().join("study.sqlite3"))?;
    let main = opening("Main", "e2e4", 0);
    let repertoire = catalog::save_opening(&db, &main, None)?;
    let after = chess::board_at(&main.route, 1)?;
    for reply in chess::legal(&after) {
        catalog::save_opening(
            &db,
            &opening("Branch", &format!("e2e4 {reply} g1f3"), 0),
            Some(&repertoire),
        )?;
    }
    let saved = catalog::current_opening_reply(&db, &repertoire, &position(&main, 0)?)?.unwrap();
    let address = issue(&db, &saved.id)?;
    answer(&mut db, &address)?;
    let (basis, _) = current(&db, &address)?;
    let edit = Edit {
        basis,
        action: Action::Remove,
    };
    let replacement = plan(&db, &edit)?;
    assert_eq!(replacement.impact.len(), 21);
    assert!(replacement.over_budget);
    let command = edit.command()?;
    let before = db.total_changes();
    assert!(
        commit(&mut db, &edit, &command, &replacement)
            .unwrap_err()
            .is::<opening_replacement::OverBudget>()
    );
    assert_eq!(db.total_changes(), before);
    assert!(opening_replacement::receipt(&db, &command)?.is_none());
    assert_eq!(current(&db, &address)?.0, edit.basis);
    Ok(())
}
