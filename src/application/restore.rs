//! Restore portable content/learning into an empty store with a fresh identity.
use super::Tempo;
use crate::{AppError, SnapshotCodec, app_error::domain};
use tempo_chess::ContentKind;
use tempo_learning::DailyLearning;
use tempo_store::Write;
use tempo_types::DatasetId;

impl Tempo {
    /// Decode and validate a portable snapshot, then atomically restore it.
    ///
    /// The destination must be empty, settings must match, and the fresh dataset
    /// identity must pass Store's checks. For edits within a dataset use `import_pgn`.
    pub fn restore(
        &mut self,
        fresh_dataset: DatasetId,
        data: &str,
        codec: &dyn SnapshotCodec,
    ) -> Result<(), AppError> {
        let snapshot = self.store.snapshot()?;
        if !snapshot.catalog.items.is_empty() {
            return Err(AppError::new(
                "restore_requires_empty",
                "restore targets an empty destination; content import is separate",
            ));
        }
        let mut portable = codec.decode(data)?;
        if !matches!(portable.schema_version, 1..=4)
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
                if self.daily.is_none()
                    || !matches!(item.definition.kind, ContentKind::Repertoire { .. })
                {
                    return Err(AppError::new(
                        "invalid_learning",
                        "Legacy game obligations require a Daily Study repertoire target.",
                    ));
                }
                DailyLearning::restore_portable(state, item.version)
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
        )?;
        self.remaining_block.clear();
        self.order_history.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{Components, JsonSnapshotCodec, Tempo};
    use tempo_chess::Catalog;
    use tempo_store::MemoryStore;
    use tempo_types::DatasetId;

    #[test]
    fn restore_uses_an_empty_destination_and_a_new_dataset_identity() {
        let source = Tempo::demo().unwrap();
        let data = source.export(&JsonSnapshotCodec).unwrap();
        let components = Components::fixtures();
        let empty = MemoryStore::new(
            "destination-before-restore".into(),
            Catalog::default(),
            components.learning.settings(),
        )
        .unwrap();
        let mut destination = Tempo::new(Box::new(empty), components).unwrap();
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
