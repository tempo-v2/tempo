//! Restore portable content/learning into an empty store with a fresh identity.
use super::Tempo;
use crate::{AppError, SnapshotCodec, app_error::domain};
use tempo_chess::ContentKind;
use tempo_learning::DailyLearning;
use tempo_store::{PortableChoiceFormat, Write};
use tempo_types::DatasetId;

impl Tempo {
    /// Decode and validate a portable snapshot, then atomically restore it.
    ///
    /// The destination must be empty, settings must match, and the fresh dataset
    /// identity must pass Store's checks. Edits within a dataset use the addressed content commands.
    pub fn restore(
        &mut self,
        fresh_dataset: DatasetId,
        data: &str,
        codec: &dyn SnapshotCodec,
    ) -> Result<(), AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.restore.restore");
        let snapshot = self.store.snapshot()?;
        if !snapshot.catalog.items.is_empty() {
            return Err(AppError::new(
                "restore_requires_empty",
                "restore targets an empty destination; content import is separate",
            ));
        }
        let mut portable = codec.decode(data)?;
        if !matches!(portable.schema_version, 1..=5)
            || portable.policy != self.components.learning.settings()
        {
            return Err(AppError::new(
                "incompatible_snapshot",
                "schema or scheduling policy differs",
            ));
        }
        if fresh_dataset.0.is_empty() {
            return Err(AppError::new(
                "invalid_dataset",
                "dataset identity is required",
            ));
        }
        for (target, state) in &mut portable.learning {
            let item = portable.catalog.items.get(target).ok_or_else(|| {
                AppError::new("invalid_learning", "Learning refers to a missing target.")
            })?;
            if state
                .correction
                .as_ref()
                .is_some_and(|correction| correction.answer_version != item.version)
            {
                return Err(AppError::new(
                    "invalid_learning",
                    "Game review refers to a different answer version.",
                ));
            }
            if state.game_review.is_some()
                || state.game_teaching_relevance
                || state.restored_game_obligation.is_some()
            {
                if !matches!(item.definition.kind, ContentKind::Repertoire { .. }) {
                    return Err(AppError::new(
                        "invalid_learning",
                        "Legacy game obligations require a Daily Study repertoire target.",
                    ));
                }
                crate::diagnostics::measure("learning.restore_portable", || {
                    DailyLearning::restore_portable(state, item.version)
                })
                .map_err(|e| domain("invalid_learning", e))?;
            }
            self.components
                .learning
                .validate_state(state)
                .map_err(|e| domain("invalid_learning", e))?;
        }
        self.store.restore_with_game_evidence(
            tempo_store::Commit {
                expected_dataset: snapshot.dataset,
                expected_revision: snapshot.revision,
                writes: vec![Write::Restore {
                    dataset: fresh_dataset,
                    catalog: portable.catalog,
                    learning: portable.learning,
                    policy: portable.policy,
                }],
            },
            &portable.game_evidence,
            if portable.schema_version == 5 {
                PortableChoiceFormat::Attributed
            } else {
                PortableChoiceFormat::Legacy
            },
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{JsonSnapshotCodec, Tempo};
    use tempo_chess::Catalog;
    use tempo_types::DatasetId;

    #[test]
    fn restore_uses_an_empty_destination_and_a_new_dataset_identity() {
        let (source, _source_directory) = crate::components::test_study();
        let data = source.export(&JsonSnapshotCodec).unwrap();
        let components = crate::components::test_components();
        let directory = tempfile::tempdir().unwrap();
        let empty = crate::components::test_store(
            &directory,
            "destination-before-restore",
            Catalog::default(),
            &components,
        );
        let mut destination = Tempo::daily(
            Box::new(empty),
            components,
            Default::default(),
            [42; 32],
            10,
        )
        .unwrap();
        destination
            .restore("restored".into(), &data, &JsonSnapshotCodec)
            .unwrap();
        let snapshot = destination.snapshot().unwrap();
        assert_eq!(snapshot.dataset, DatasetId::from("restored"));
        assert_eq!(snapshot.catalog, source.snapshot().unwrap().catalog);
        assert!(snapshot.attempts.is_empty());
        assert!(destination.next_exercise().unwrap().is_some());
    }
}
