//! Repertoire, multi-decision tactics, correction, and abandonment.
use super::support::{answer, one, start};
use tempo::wire::{StudyCommand, StudyView};
use tempo::{AbandonCommand, SubmitCommand};
use tempo_learning::LearningState;
use tempo_study::AttemptStatus;
use tempo_types::{Grade, StudyDay, TargetId};

#[test]
fn repertoire_due_journey_crosses_wire_queue_grader_learning_and_store() {
    let mut h = one("r1:p1");
    let StudyView::Attempt(view) = h.app.handle(StudyCommand::Next) else {
        panic!("expected attempt view")
    };
    assert_eq!(view.target_id, TargetId::from("r1:p1"));
    assert_eq!(view.status, "ready");
    let rendered = serde_json::to_value(&view).unwrap();
    assert!(rendered.get("state").is_none());
    assert!(rendered["prompt"].get("expected").is_none());
    let intent = SubmitCommand {
        dataset_id: view.dataset_id,
        attempt_id: view.attempt_id,
        submission_id: "wire-answer".into(),
        expected_revision: view.revision,
        answer: "e2e4".into(),
    };
    let encoded = serde_json::to_string(&StudyCommand::Answer(intent)).unwrap();
    let StudyView::Attempt(done) = h.app.handle(serde_json::from_str(&encoded).unwrap()) else {
        panic!("expected completion")
    };
    assert_eq!(done.status, "complete");
    assert_eq!(done.assessment, Some(Grade::Good));
    let saved = h.app.snapshot().unwrap();
    assert_eq!(saved.assessments.len(), 1);
    let learning = &saved.learning[&TargetId::from("r1:p1")];
    assert_eq!(learning.last_graded_day, Some(StudyDay(10)));
    assert_eq!(learning.scheduled_reviews, 1);
    assert!(learning.scheduler_memory.is_some());
    assert!(matches!(
        h.app.handle(StudyCommand::Next),
        StudyView::Empty(_)
    ));
}

#[test]
fn two_decision_tactic_has_one_grade_and_autoplay_has_none() {
    let mut h = one("t1");
    let issued = start(&mut h);
    let first = answer(&mut h, &issued, "t-first", "e2e4");
    assert_eq!(first.assessment, None);
    assert_eq!(first.attempt.prompt.as_ref().unwrap().decision_index, 1);
    assert_eq!(
        first.feedback.as_ref().unwrap().autoplay,
        vec!["e7e5".into()]
    );
    let half = h.app.snapshot().unwrap();
    assert!(half.assessments.is_empty());
    assert!(!half.learning[&TargetId::from("t1")].introduced);
    let final_answer = answer(&mut h, &first, "t-final", "g1f3");
    assert_eq!(final_answer.assessment, Some(Grade::Good));
    assert_eq!(final_answer.attempt.status, AttemptStatus::Complete);
    assert_eq!(h.app.snapshot().unwrap().assessments.len(), 1);
}

#[test]
fn first_miss_is_committed_and_correction_stays_failed_without_a_second_grade() {
    let mut h = one("t1");
    let issued = start(&mut h);
    let missed = answer(&mut h, &issued, "miss", "e2e3");
    assert_eq!(missed.assessment, Some(Grade::Again));
    assert_eq!(missed.attempt.status, AttemptStatus::Active);
    let persisted = h.app.snapshot().unwrap();
    assert_eq!(persisted.assessments.len(), 1);
    assert_eq!(
        persisted.learning[&TargetId::from("t1")].scheduled_reviews,
        1
    );
    assert_eq!(
        h.app.resume(&issued.attempt.id).unwrap().attempt.terminal,
        Some(Grade::Again)
    );
    let corrected = answer(&mut h, &missed, "correct-1", "e2e4");
    let finished = answer(&mut h, &corrected, "correct-2", "g1f3");
    assert_eq!(finished.attempt.terminal, Some(Grade::Again));
    assert_eq!(finished.assessment, None);
    assert_eq!(h.app.snapshot().unwrap().assessments.len(), 1);
    assert_eq!(
        h.app.snapshot().unwrap().learning[&TargetId::from("t1")],
        persisted.learning[&TargetId::from("t1")],
    );
}

#[test]
fn abandonment_is_replayable_without_a_grade_and_a_fresh_attempt_can_start() {
    let mut h = one("t1");
    let issued = start(&mut h);
    let abandon = AbandonCommand {
        dataset_id: issued.attempt.dataset,
        attempt_id: issued.attempt.id.clone(),
        submission_id: "abandon".into(),
        expected_revision: issued.attempt.revision,
    };
    let ended = h.app.abandon(abandon.clone()).unwrap();
    assert_eq!(ended.attempt.status, AttemptStatus::Abandoned);
    assert_eq!(ended.assessment, None);
    let snapshot = h.app.snapshot().unwrap();
    assert!(snapshot.assessments.is_empty());
    assert_eq!(
        snapshot.learning[&TargetId::from("t1")],
        LearningState::default()
    );
    assert_eq!(h.app.abandon(abandon).unwrap(), ended);
    assert_eq!(h.app.snapshot().unwrap(), snapshot);
    assert_ne!(start(&mut h).attempt.id, issued.attempt.id);
}
