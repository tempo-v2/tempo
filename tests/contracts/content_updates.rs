//! Metadata, semantic edits, and invalid/quarantined content.
use super::support::{
    answer, at, build, catalog, command, external, import_request, seed_due, start,
};
use tempo_chess::{ChangeKind, ContentStatus, FixtureParser, demo_catalog};
use tempo_learning::LearningState;
use tempo_types::{ContentVersion, Grade, TargetId};

#[test]
fn metadata_import_preserves_learning_version_and_pending_attempt() {
    let mut metadata = catalog(&["r1:p1"]);
    metadata
        .items
        .get_mut(&TargetId::from("r1:p1"))
        .unwrap()
        .label = "Updated explanation".into();
    let mut h = build(catalog(&["r1:p1"]), |c| {
        c.parser = Box::new(FixtureParser::default().with_fixture("metadata", metadata));
    });
    seed_due(&mut h, &["r1:p1"]);
    let issued = start(&mut h);
    let old_learning = h.app.snapshot().unwrap().learning;
    let changes = h
        .app
        .import_pgn(&issued.attempt.dataset, &import_request("metadata"))
        .unwrap();
    assert!(changes.iter().any(|c| c.kind == ChangeKind::Metadata));
    let snapshot = h.app.snapshot().unwrap();
    assert_eq!(snapshot.learning, old_learning);
    assert_eq!(
        snapshot.catalog.items[&TargetId::from("r1:p1")].version,
        ContentVersion(1)
    );
    assert_eq!(
        answer(&mut h, &issued, "after-metadata", "e2e4").assessment,
        Some(Grade::Good)
    );
}

#[test]
fn semantic_update_rejects_pending_and_stale_game_work_but_replays_committed_receipt() {
    let mut changed = catalog(&["r1:p1"]);
    changed
        .items
        .get_mut(&TargetId::from("r1:p1"))
        .unwrap()
        .definition
        .decisions[0]
        .expected = "d2d4".into();
    let mut h = build(catalog(&["r1:p1"]), |c| {
        c.parser = Box::new(FixtureParser::default().with_fixture("replacement", changed));
    });
    let issued = start(&mut h);
    let committed_command = command(&issued, "accepted-old-content", "e2e4");
    let accepted = h.app.submit(committed_command.clone()).unwrap();
    h.clock.set(at(13));
    let pending = start(&mut h);
    let stale_game = external(&h, "r1:p1", "compared-before-edit");
    h.app
        .import_pgn(&pending.attempt.dataset, &import_request("replacement"))
        .unwrap();
    let replacement = h.app.snapshot().unwrap();
    assert_eq!(
        replacement.catalog.items[&TargetId::from("r1:p1")].version,
        ContentVersion(2)
    );
    assert_eq!(
        replacement.learning[&TargetId::from("r1:p1")],
        LearningState::default()
    );
    assert_eq!(
        h.app
            .submit(command(&pending, "pending-old-content", "e2e4"))
            .unwrap_err()
            .code,
        "stale_content"
    );
    assert_eq!(h.app.again(stale_game).unwrap_err().code, "stale_content");
    assert_eq!(h.app.submit(committed_command).unwrap(), accepted);
    assert_eq!(h.app.snapshot().unwrap(), replacement);
    assert_eq!(replacement.assessments.len(), 1);
    assert_eq!(replacement.content_history.len(), 2);
    let new = start(&mut h);
    assert_eq!(
        answer(&mut h, &new, "new-content", "d2d4").assessment,
        Some(Grade::Good)
    );
}

#[test]
fn unknown_content_is_rejected_and_quarantined_items_do_not_block_valid_study() {
    let mut content = demo_catalog();
    content.items.get_mut(&TargetId::from("t1")).unwrap().status = ContentStatus::Quarantined;
    let mut h = build(content, |_| {});
    let before = h.app.snapshot().unwrap();
    assert_eq!(
        h.app
            .import_pgn(&before.dataset, &import_request("not a registered fixture"))
            .unwrap_err()
            .code,
        "content_parse_error"
    );
    assert_eq!(h.app.snapshot().unwrap(), before);
    assert_eq!(start(&mut h).attempt.target, TargetId::from("r1:p1"));
    let mut only_quarantined = catalog(&["t1"]);
    only_quarantined
        .items
        .get_mut(&TargetId::from("t1"))
        .unwrap()
        .status = ContentStatus::Quarantined;
    let mut empty = build(only_quarantined, |_| {});
    assert!(empty.app.next_exercise().unwrap().is_none());
}
