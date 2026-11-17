//! A different ordering policy preserves contiguous repertoire blocks.
use super::support::{answer, build, external, seed_due, start};
use tempo_chess::demo_catalog;
use tempo_queue::ReverseQueue;
use tempo_types::TargetId;

#[test]
fn queue_policy_substitution_changes_selection_and_preserves_group_contiguity() {
    let mut stable = build(demo_catalog(), |_| {});
    let mut reverse = build(demo_catalog(), |c| c.queue = Box::new(ReverseQueue));
    assert_eq!(start(&mut stable).attempt.target, TargetId::from("r1:p1"));
    assert_eq!(start(&mut reverse).attempt.target, TargetId::from("t1"));
    assert_eq!(
        stable.app.snapshot().unwrap().learning,
        reverse.app.snapshot().unwrap().learning
    );
    let mut grouped = build(demo_catalog(), |_| {});
    seed_due(&mut grouped, &["r1:p1", "r1:p2", "t1"]);
    let first = start(&mut grouped);
    assert_eq!(first.attempt.target, TargetId::from("r1:p1"));
    answer(&mut grouped, &first, "group-first", "e2e4");
    grouped
        .app
        .again(external(&grouped, "t1", "new-priority-miss"))
        .unwrap();
    assert_eq!(start(&mut grouped).attempt.target, TargetId::from("r1:p2"));
}
