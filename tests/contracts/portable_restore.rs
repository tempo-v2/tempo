//! Export/restore compatibility and preserved future learning behavior.
use super::support::{answer, at, build, command, import_request, one, start};
use tempo::{JsonSnapshotCodec, SnapshotCodec};
use tempo_chess::Catalog;
use tempo_types::{DatasetId, TargetId};

#[test]
fn ordinary_import_preserves_progress_and_portable_restore_preserves_next_grade() {
    let mut source = one("r1:p1");
    let issued = start(&mut source);
    let missed = answer(&mut source, &issued, "miss-before-export", "d2d4");
    answer(&mut source, &missed, "correction-before-export", "e2e4");
    let fresh = start(&mut source);
    answer(&mut source, &fresh, "repeat-before-export", "e2e4");
    let learning = source.app.snapshot().unwrap().learning;
    let dataset = source.app.snapshot().unwrap().dataset;
    source
        .app
        .import_pgn(&dataset, &import_request(tempo_chess::REPERTOIRE_PGN))
        .unwrap();
    assert_eq!(
        source.app.snapshot().unwrap().learning[&TargetId::from("r1:p1")],
        learning[&TargetId::from("r1:p1")]
    );
    let portable = source.app.export(&JsonSnapshotCodec).unwrap();
    let mut destination = build(Catalog::default(), |_| {});
    destination
        .app
        .restore("restored-dataset".into(), &portable, &JsonSnapshotCodec)
        .unwrap();
    let restored = destination.app.snapshot().unwrap();
    assert_eq!(restored.catalog, source.app.snapshot().unwrap().catalog);
    assert_eq!(restored.learning, source.app.snapshot().unwrap().learning);
    assert_eq!(restored.dataset, DatasetId::from("restored-dataset"));
    assert!(restored.attempts.is_empty());
    source.clock.set(at(11));
    destination.clock.set(at(11));
    let from_source = start(&mut source);
    let from_restore = start(&mut destination);
    assert_eq!(from_source.attempt.target, from_restore.attempt.target);
    let mut old_dataset = command(&from_restore, "old-dataset-command", "e2e4");
    old_dataset.dataset_id = dataset;
    assert_eq!(
        destination.app.submit(old_dataset).unwrap_err().code,
        "stale_dataset"
    );
    answer(&mut source, &from_source, "after-export", "e2e4");
    answer(&mut destination, &from_restore, "after-restore", "e2e4");
    assert_eq!(
        source.app.snapshot().unwrap().learning,
        destination.app.snapshot().unwrap().learning
    );
}

#[test]
fn restore_rejects_bad_settings_memory_and_reused_dataset_before_mutation() {
    let mut source = one("r1:p1");
    let issued = start(&mut source);
    answer(&mut source, &issued, "snapshot-grade", "e2e4");
    let encoded = source.app.export(&JsonSnapshotCodec).unwrap();
    let portable = JsonSnapshotCodec.decode(&encoded).unwrap();
    let mut destination = build(Catalog::default(), |_| {});
    let before = destination.app.snapshot().unwrap();
    let mut wrong_policy = portable.clone();
    wrong_policy.policy.parameters = 9u32.to_le_bytes().to_vec();
    let error = destination
        .app
        .restore(
            "fresh".into(),
            &JsonSnapshotCodec.encode(&wrong_policy).unwrap(),
            &JsonSnapshotCodec,
        )
        .unwrap_err();
    assert_eq!(error.code, "incompatible_snapshot");
    assert_eq!(destination.app.snapshot().unwrap(), before);
    let mut wrong_memory = portable;
    wrong_memory
        .learning
        .get_mut(&TargetId::from("r1:p1"))
        .unwrap()
        .scheduler_memory
        .as_mut()
        .unwrap()
        .schema_version = 99;
    assert_eq!(
        destination
            .app
            .restore(
                "fresh".into(),
                &JsonSnapshotCodec.encode(&wrong_memory).unwrap(),
                &JsonSnapshotCodec
            )
            .unwrap_err()
            .code,
        "invalid_learning"
    );
    assert_eq!(destination.app.snapshot().unwrap(), before);
    assert_eq!(
        destination
            .app
            .restore(before.dataset.clone(), &encoded, &JsonSnapshotCodec)
            .unwrap_err()
            .code,
        "store_conflict"
    );
    assert_eq!(destination.app.snapshot().unwrap(), before);
    destination
        .app
        .restore("fresh".into(), &encoded, &JsonSnapshotCodec)
        .unwrap();
    assert_eq!(
        destination
            .app
            .restore("another".into(), &encoded, &JsonSnapshotCodec)
            .unwrap_err()
            .code,
        "restore_requires_empty"
    );
}
