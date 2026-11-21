//! Apply identified external feedback without inventing a human study attempt.
use super::{Tempo, receipts::replay};
use crate::{AgainCommand, AppError, app_error::domain};
use tempo_chess::ContentKind;
use tempo_learning::{AssessmentInput, AssessmentSource, Transition};
use tempo_queue::OrderEvent;
use tempo_store::{AssessmentKey, AssessmentRecord, Receipt, ReceiptResult, StudyScope, Write};
use tempo_types::Grade;

impl Tempo {
    /// Apply or replay one externally observed miss.
    ///
    /// The event is tied to the compared content version. Only a newly committed
    /// event updates queue history; replay returns the saved learning transition.
    pub fn again(&mut self, command: AgainCommand) -> Result<Transition, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.again.again");
        let fingerprint =
            serde_json::to_string(&("again", &command)).map_err(|e| domain("codec_error", e))?;
        let submission =
            Receipt::external_id(&command.dataset_id, &command.provider, &command.event_id);
        let scope = StudyScope {
            target: Some(&command.target),
            submission: Some(&submission),
            ..Default::default()
        };
        let snapshot = self.store.study_state(scope)?;
        if let Some(result) = replay(&snapshot, &submission, &fingerprint)? {
            return match result {
                ReceiptResult::External { transition, .. } => Ok(transition),
                _ => Err(AppError::new("submission_conflict", "receipt kind differs")),
            };
        }
        if command.dataset_id != snapshot.dataset {
            return Err(AppError::new(
                "stale_dataset",
                "command belongs to a different dataset",
            ));
        }
        if command.provider.is_empty() || command.event_id.0.is_empty() {
            return Err(AppError::new(
                "invalid_event",
                "source and event identity are required",
            ));
        }
        let (item, learning) = snapshot
            .target(&command.target)?
            .ok_or_else(|| AppError::new("unknown_target", "feedback target does not exist"))?;
        if item.version != command.expected_content_version {
            return Err(AppError::new(
                "stale_content",
                "feedback was computed against a different content version",
            ));
        }
        if !item.active() {
            return Err(AppError::new(
                "inactive_target",
                "feedback target is quarantined",
            ));
        }
        if super::repertoires::is_suspended(item, &snapshot.suspended_repertoires) {
            return Err(super::repertoires::suspended_repertoire());
        }
        if let ContentKind::Repertoire { repertoire, .. } = &item.definition.kind
            && self
                .store
                .repertoire_reset_at(&snapshot.dataset, repertoire)?
                .is_some_and(|reset| command.observed_at_ms <= reset)
        {
            return Err(AppError::new(
                "before_progress_reset",
                "This observation predates the repertoire's progress reset.",
            ));
        }
        let input = AssessmentInput {
            grade: Grade::Again,
            accepted_at: self.components.clock.now(),
            source: AssessmentSource::External {
                provider: command.provider.clone(),
                event_id: command.event_id.clone(),
                observed_at_ms: command.observed_at_ms,
            },
        };
        let transition = self
            .components
            .learning
            .apply(learning, &input)
            .map_err(|e| domain("learning_error", e))?;
        self.commit_study(
            &snapshot,
            vec![
                Write::Learning {
                    target: command.target.clone(),
                    state: transition.state.clone(),
                },
                Write::Assessment(AssessmentRecord {
                    key: AssessmentKey::External {
                        provider: command.provider,
                        event_id: command.event_id,
                    },
                    dataset: snapshot.dataset.clone(),
                    target: command.target.clone(),
                    content_version: item.version,
                    input,
                    transition: transition.clone(),
                }),
                Write::Receipt(Receipt {
                    id: submission.clone(),
                    command: fingerprint,
                    result: ReceiptResult::External {
                        target: command.target.clone(),
                        transition: transition.clone(),
                    },
                }),
            ],
            scope,
        )?;
        self.order_history.push(OrderEvent::Missed {
            target: command.target,
        });
        Ok(transition)
    }
}

#[cfg(test)]
mod tests {
    use crate::{AgainCommand, Tempo};
    use tempo_types::TargetId;

    #[test]
    fn an_external_miss_updates_learning_without_creating_a_human_attempt() {
        let mut app = Tempo::demo().unwrap();
        let snapshot = app.snapshot().unwrap();
        let target = TargetId::from("r1:p1");
        app.again(AgainCommand {
            dataset_id: snapshot.dataset,
            expected_content_version: snapshot.catalog.items[&target].version,
            target: target.clone(),
            provider: "game-provider".into(),
            event_id: "42:position-1".into(),
            observed_at_ms: 0,
        })
        .unwrap();
        let saved = app.snapshot().unwrap();
        assert!(saved.learning[&target].repeat_pending);
        assert!(saved.attempts.is_empty());
        assert_eq!(saved.assessments.len(), 1);
    }
}
