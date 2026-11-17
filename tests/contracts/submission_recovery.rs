//! Lost replies, stale revisions, and atomic failure/retry.
use super::support::{answer, command, one, start};
use tempo_types::Grade;

#[test]
fn active_resume_replay_and_revision_checks_prevent_double_answers() {
    let mut h = one("t1");
    let issued = start(&mut h);
    let before = h.app.snapshot().unwrap();
    assert_eq!(h.app.next_exercise().unwrap().unwrap(), issued);
    assert_eq!(h.app.resume(&issued.attempt.id).unwrap(), issued);
    assert_eq!(h.app.snapshot().unwrap(), before);
    let first_command = command(&issued, "same-command", "e2e4");
    let first = h.app.submit(first_command.clone()).unwrap();
    let accepted = h.app.snapshot().unwrap();
    assert_eq!(h.app.submit(first_command.clone()).unwrap(), first);
    assert_eq!(h.app.snapshot().unwrap(), accepted);
    let mut conflicting = first_command.clone();
    conflicting.answer = "d2d4".into();
    assert_eq!(
        h.app.submit(conflicting).unwrap_err().code,
        "submission_conflict"
    );
    let mut stale_question = first_command;
    stale_question.submission_id = "different-command-old-question".into();
    assert_eq!(
        h.app.submit(stale_question).unwrap_err().code,
        "invalid_submission"
    );
    let completed = answer(&mut h, &first, "finish", "g1f3");
    assert_eq!(
        h.app
            .submit(command(&completed, "second-terminal-id", "g1f3"))
            .unwrap_err()
            .code,
        "invalid_submission"
    );
    assert_eq!(h.app.snapshot().unwrap().assessments.len(), 1);
}

#[test]
fn failed_commit_leaves_progress_grade_learning_and_receipt_unchanged_then_retry_works() {
    let mut h = one("t1");
    let issued = start(&mut h);
    let before = h.app.snapshot().unwrap();
    let submit = command(&issued, "retry-after-failure", "e2e3");
    h.store.borrow_mut().fail_next_commit();
    let failure = h.app.submit(submit.clone()).unwrap_err();
    assert_eq!(failure.code, "store_unavailable");
    assert!(failure.retryable);
    assert_eq!(h.app.snapshot().unwrap(), before);
    let accepted = h.app.submit(submit.clone()).unwrap();
    assert_eq!(accepted.assessment, Some(Grade::Again));
    let saved = h.app.snapshot().unwrap();
    assert_eq!(saved.assessments.len(), 1);
    assert_eq!(saved.receipts.len(), 1);
    assert_eq!(h.app.submit(submit).unwrap(), accepted);
    assert_eq!(h.app.snapshot().unwrap(), saved);
}
