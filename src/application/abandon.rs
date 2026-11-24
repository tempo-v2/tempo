//! Stop an attempt and save the reply for safe retries.
use super::{
    Tempo,
    receipts::{replay_accepted_study, replay_study_in_scope},
};
use crate::{
    AbandonCommand, AppError,
    app_error::{domain, study_input},
};
use tempo_store::{
    ActivityMode, AnswerResult, Commit, Receipt, ReceiptResult, StoredAttempt, StudyScope,
    StudyState, Write,
};

use tempo_types::AttemptId;

pub(super) fn retired_warmup() -> AppError {
    AppError::new(
        "retired_activity",
        "Connected repertoire warm-ups have been retired.",
    )
}

impl Tempo {
    /// Stop an attempt without producing a new assessment.
    ///
    /// Abandonment clears its active pointer and saves a receipt atomically. An
    /// assessment already emitted by an earlier miss remains part of the attempt.
    pub fn abandon(&mut self, command: AbandonCommand) -> Result<AnswerResult, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.abandon.abandon");
        self.leave(command, false)
    }

    pub(super) fn skip(&mut self, command: AbandonCommand) -> Result<AnswerResult, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.abandon.skip");
        self.leave(command, true)
    }

    /// Next can retire previously issued Practice or a warm-up without grading it.
    pub(super) fn retire_legacy_active(
        &mut self,
        snapshot: &StudyState,
        id: &AttemptId,
    ) -> Result<bool, AppError> {
        if snapshot.active_attempt.as_ref() != Some(id) {
            return Ok(false);
        }
        let Some(record) = snapshot
            .attempts
            .get(id)
            .filter(|record| record.repertoire_warmup || record.mode == ActivityMode::Practice)
        else {
            return Ok(false);
        };
        let attempt =
            tempo_study::abandon(&record.attempt, record.attempt.revision).map_err(study_input)?;
        self.commit_study(
            snapshot,
            vec![
                Write::Progress(StoredAttempt {
                    attempt,
                    ..record.clone()
                }),
                Write::Active(None),
            ],
            StudyScope::default(),
        )?;
        Ok(true)
    }

    fn leave(&mut self, command: AbandonCommand, skip: bool) -> Result<AnswerResult, AppError> {
        let scope = StudyScope {
            attempt: Some(&command.attempt_id),
            submission: Some(&command.submission_id),
            ..StudyScope::default()
        };
        let action = if skip { "skip" } else { "abandon" };
        let fingerprint =
            serde_json::to_string(&(action, &command)).map_err(|e| domain("codec_error", e))?;
        if let Some(result) = replay_accepted_study(
            self.store.as_ref(),
            &command.dataset_id,
            &command.submission_id,
            &fingerprint,
        )? {
            return Ok(result);
        }
        // Recheck the receipt in the coherent new-work read. Another delivery
        // may have committed it since the addressed lookup.
        let snapshot = self.store.study_state(scope)?;
        if let Some(result) = replay_study_in_scope(
            &snapshot,
            &command.dataset_id,
            &command.submission_id,
            &fingerprint,
        )? {
            return Ok(result);
        }
        let old = self.current(&snapshot, &command.dataset_id, &command.attempt_id)?;
        if old.repertoire_warmup {
            return Err(retired_warmup());
        }
        let attempt = crate::diagnostics::measure("study.abandon", || {
            tempo_study::abandon(&old.attempt, command.expected_revision)
        })
        .map_err(study_input)?;
        let result = AnswerResult {
            attempt: attempt.clone(),
            feedback: None,
            assessment: None,
            learning_effect: None,
        };
        let mut writes = vec![
            Write::Progress(StoredAttempt {
                attempt,
                ..old.clone()
            }),
            Write::Receipt(Receipt {
                id: command.submission_id.clone(),
                command: fingerprint,
                result: ReceiptResult::Study(result.clone()),
            }),
        ];
        if snapshot.active_attempt.as_ref() == Some(&command.attempt_id) {
            writes.push(Write::Active(None));
        }
        let at = skip.then(|| self.components.clock.now());
        // Store resolves the exact obligation from the immutable issue and
        // any assessment bound to this attempt in the Skip transaction.
        if let Some(at) = at.filter(|_| old.issued_work.is_some()) {
            self.store.commit_issued_skip(
                Commit {
                    expected_dataset: snapshot.dataset.clone(),
                    expected_revision: snapshot.revision,
                    writes,
                },
                scope,
                at.day,
            )?;
        } else {
            self.commit_study(&snapshot, writes, scope)?;
            // This is the unchanged legacy path for attempts issued before an
            // exact token is available. The prepared Next cutover removes it.
            if let (Some(at), Some(daily)) = (at, &mut self.daily) {
                daily.queue.defer(result.attempt.target.clone(), at.day);
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use crate::AbandonCommand;

    #[test]
    fn missed_exact_test_skip_replays_after_restart() {
        use crate::{IntroductionLimits, Tempo, components, wire::StudyCommand};
        use serde_json::{Value, json};
        use tempo_store::{Commit, SqliteStore, Store, Write};
        use tempo_types::{WorkKind, WorkObligation, WorkToken};

        let study = |app: &mut Tempo, command: Value| {
            let command: StudyCommand = serde_json::from_value(command).unwrap();
            serde_json::to_value(app.handle(command)).unwrap()
        };
        for first_command in ["answer", "give_up"] {
            let (mut app, directory) = components::test_study();
            let legacy = app.next_exercise().unwrap().unwrap();
            let mut issued = app.snapshot().unwrap().attempts[&legacy.attempt.id].clone();
            app.abandon(AbandonCommand {
                dataset_id: legacy.attempt.dataset,
                attempt_id: legacy.attempt.id,
                expected_revision: legacy.attempt.revision,
                submission_id: "legacy-stop".into(),
            })
            .unwrap();
            drop(app);

            let path = directory.path().join("study.sqlite");
            let mut store = SqliteStore::open(&path).unwrap();
            let before = store.snapshot().unwrap();
            assert_eq!(issued.mode, tempo_store::ActivityMode::Test);
            issued.attempt.id = "exact-test".into();
            issued.issued_revision = Some(before.revision);
            let target = issued.attempt.target.clone();
            let dataset = before.dataset.clone();
            let token = WorkToken {
                day: issued.issued_at.day,
                obligation: WorkObligation {
                    target: target.clone(),
                    version: issued.attempt.content_version,
                    revision: issued.issued_miss_revision,
                    kind: WorkKind::Test,
                },
            };
            issued.issued_work = Some(token.clone());
            let attempt_id = issued.attempt.id.clone();
            store
                .commit(Commit {
                    expected_dataset: dataset.clone(),
                    expected_revision: before.revision,
                    writes: vec![
                        Write::Issue(issued),
                        Write::Active(Some(attempt_id.clone())),
                    ],
                })
                .unwrap();
            let open = |store| {
                Tempo::daily(
                    Box::new(store),
                    components::test_components(),
                    IntroductionLimits::default(),
                    [42; 32],
                    10,
                )
                .unwrap()
            };
            let mut app = open(store);
            let mut first = json!({
                "command": first_command, "dataset_id": dataset, "attempt_id": attempt_id,
                "submission_id": "first-miss", "expected_revision": 0
            });
            if first_command == "answer" {
                first["answer"] = json!({ "uci": "a2a3" });
            }
            let missed = study(&mut app, first);
            assert_eq!(missed["status"], "ready", "{missed}");
            let before_skip = app.snapshot().unwrap();
            let bound = before_skip.assessments[0].transition.state.miss_revision;
            assert_eq!(bound.0, token.obligation.revision.0 + 1);
            let skip = json!({
                "command": "skip", "dataset_id": dataset, "attempt_id": attempt_id,
                "submission_id": "correction-skip", "expected_revision": missed["revision"],
            });
            drop(app);
            let mut app = open(SqliteStore::open(&path).unwrap());
            let skipped = study(&mut app, skip.clone());
            assert_eq!(skipped["status"], "abandoned", "{skipped}");
            let settled = app.snapshot().unwrap();
            assert_eq!(settled.active_attempt, None);
            assert_eq!(settled.assessments, before_skip.assessments);
            assert_eq!(settled.learning, before_skip.learning);
            assert_eq!(
                settled.attempts[&attempt_id].issued_work,
                Some(token.clone())
            );
            drop(app);

            let mut app = open(SqliteStore::open(&path).unwrap());
            let before_replay = app.snapshot().unwrap();
            assert_eq!(study(&mut app, skip), skipped);
            assert_eq!(app.snapshot().unwrap(), before_replay);
        }
    }

    #[test]
    fn abandoning_clears_the_active_pointer_and_allows_a_fresh_attempt() {
        let (mut app, _directory) = crate::components::test_study();
        let issued = app.next_exercise().unwrap().unwrap();
        app.abandon(AbandonCommand {
            dataset_id: issued.attempt.dataset,
            attempt_id: issued.attempt.id.clone(),
            submission_id: "stop".into(),
            expected_revision: issued.attempt.revision,
        })
        .unwrap();
        assert_eq!(app.snapshot().unwrap().active_attempt, None);
        let replacement = app.next_exercise().unwrap().unwrap();
        assert_ne!(replacement.attempt.id, issued.attempt.id);
        assert_eq!(replacement.attempt.target, issued.attempt.target);
    }
}
