//! Export/restore compatibility and preserved future learning behavior.
use super::support::{answer, empty_store, one, open, start};
use tempo::{JsonSnapshotCodec, SnapshotCodec};
use tempo_types::{DatasetId, TargetId};

#[test]
fn portable_restore_preserves_the_daily_schedule_and_changes_dataset_identity() {
    let mut source = one("r1:p1");
    let issued = start(&mut source);
    answer(&mut source, &issued, "before-export", "e2e4");
    let saved = source.app.snapshot().unwrap();
    let portable = source.app.export(&JsonSnapshotCodec).unwrap();

    let directory = tempfile::tempdir().unwrap();
    let mut destination = open(Box::new(empty_store(&directory)), 10, 0);
    destination
        .restore("restored-dataset".into(), &portable, &JsonSnapshotCodec)
        .unwrap();
    let restored = destination.snapshot().unwrap();
    assert_eq!(restored.catalog, saved.catalog);
    assert_eq!(restored.learning, saved.learning);
    assert_eq!(restored.dataset, DatasetId::from("restored-dataset"));
    assert!(restored.attempts.is_empty());
    assert!(restored.assessments.is_empty());
    assert!(restored.receipts.is_empty());
}

#[test]
fn restore_rejects_bad_settings_memory_and_reused_dataset_before_mutation() {
    let mut source = one("r1:p1");
    let issued = start(&mut source);
    answer(&mut source, &issued, "snapshot-grade", "e2e4");
    let encoded = source.app.export(&JsonSnapshotCodec).unwrap();
    let portable = JsonSnapshotCodec.decode(&encoded).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut destination = open(Box::new(empty_store(&directory)), 10, 0);
    let before = destination.snapshot().unwrap();
    let mut wrong_policy = portable.clone();
    wrong_policy.policy.parameters = 9u32.to_le_bytes().to_vec();
    let error = destination
        .restore(
            "fresh".into(),
            &JsonSnapshotCodec.encode(&wrong_policy).unwrap(),
            &JsonSnapshotCodec,
        )
        .unwrap_err();
    assert_eq!(error.code, "incompatible_snapshot");
    assert_eq!(destination.snapshot().unwrap(), before);
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
            .restore(
                "fresh".into(),
                &JsonSnapshotCodec.encode(&wrong_memory).unwrap(),
                &JsonSnapshotCodec
            )
            .unwrap_err()
            .code,
        "invalid_learning"
    );
    assert_eq!(destination.snapshot().unwrap(), before);
    assert_eq!(
        destination
            .restore(before.dataset.clone(), &encoded, &JsonSnapshotCodec)
            .unwrap_err()
            .code,
        "store_conflict"
    );
    assert_eq!(destination.snapshot().unwrap(), before);
    destination
        .restore("fresh".into(), &encoded, &JsonSnapshotCodec)
        .unwrap();
    assert_eq!(
        destination
            .restore("another".into(), &encoded, &JsonSnapshotCodec)
            .unwrap_err()
            .code,
        "restore_requires_empty"
    );
}
