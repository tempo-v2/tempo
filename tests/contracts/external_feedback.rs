//! Provider failure and replayable game feedback independent of human attempts.
use super::support::{answer, at, command, external, one, start};
use tempo::{FixtureGames, Game, GameProvider, GameRange, UnavailableGames};
use tempo_learning::{AssessmentSource, LearningEffect};
use tempo_store::AssessmentKey;
use tempo_types::{Grade, StudyDay, TargetId};

#[test]
fn programmatic_again_has_source_identity_and_no_fabricated_attempt() {
    let mut h = one("r1:p1");
    let command = external(&h, "r1:p1", "game-42");
    let applied = h.app.again(command.clone()).unwrap();
    assert_eq!(applied.effect, LearningEffect::MissRecorded);
    assert_eq!(applied.state.due, Some(StudyDay(11)));
    let snapshot = h.app.snapshot().unwrap();
    assert!(snapshot.attempts.is_empty());
    assert_eq!(snapshot.assessments.len(), 1);
    assert!(matches!(
        &snapshot.assessments[0].key,
        AssessmentKey::External { .. }
    ));
    assert!(matches!(
        &snapshot.assessments[0].input.source,
        AssessmentSource::External {
            observed_at_ms: 0,
            ..
        }
    ));
    assert_eq!(snapshot.assessments[0].input.accepted_at, at(10));
    assert_eq!(h.app.again(command.clone()).unwrap(), applied);
    assert_eq!(h.app.snapshot().unwrap(), snapshot);
    let mut reused = command;
    reused.observed_at_ms = 1;
    assert_eq!(h.app.again(reused).unwrap_err().code, "submission_conflict");
}

#[test]
fn old_human_success_cannot_clear_a_newer_external_miss() {
    let mut h = one("r1:p1");
    let issued = start(&mut h);
    h.app.again(external(&h, "r1:p1", "newer-miss")).unwrap();
    let old_command = command(&issued, "old-success", "e2e4");
    let stale = h.app.submit(old_command.clone()).unwrap();
    assert_eq!(stale.assessment, Some(Grade::Good));
    assert_eq!(
        stale.learning_effect,
        Some(LearningEffect::StalePositiveIgnored)
    );
    assert!(h.app.snapshot().unwrap().learning[&TargetId::from("r1:p1")].repeat_pending);
    let fresh = start(&mut h);
    let cleared = answer(&mut h, &fresh, "fresh-success", "e2e4");
    assert_eq!(cleared.learning_effect, Some(LearningEffect::RepeatCleared));
    let after = h.app.snapshot().unwrap();
    h.clock.set(at(11));
    assert_eq!(h.app.submit(old_command).unwrap(), stale);
    assert_eq!(h.app.snapshot().unwrap(), after);
}

#[test]
fn optional_game_failure_does_not_block_study_and_scan_uses_again_with_replay() {
    let mut unavailable = one("r1:p1");
    let range = GameRange {
        from_ms: 0,
        to_ms: 100,
    };
    assert_eq!(
        UnavailableGames.fetch(&range).unwrap_err().code,
        "provider_unavailable"
    );
    assert_eq!(
        start(&mut unavailable).attempt.target,
        TargetId::from("r1:p1")
    );
    let mut available = one("r1:p1");
    let games = FixtureGames(vec![Game {
        provider: "fixture".into(),
        id: "game-1".into(),
        observed_at_ms: 1,
        pgn: tempo_chess::GAME_PGN.into(),
    }])
    .fetch(&range)
    .unwrap();
    assert_eq!(available.app.ingest_games(&games).unwrap(), 1);
    let first = available.app.snapshot().unwrap();
    assert!(first.attempts.is_empty());
    assert_eq!(first.assessments.len(), 1);
    assert!(first.learning[&TargetId::from("r1:p1")].repeat_pending);
    assert_eq!(available.app.ingest_games(&games).unwrap(), 1);
    assert_eq!(available.app.snapshot().unwrap(), first);
}
