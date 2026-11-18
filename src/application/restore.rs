//! Restore portable content/learning into an empty store with a fresh identity.
use super::Tempo;
use crate::{AppError, SnapshotCodec, app_error::domain};
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
        let portable = codec.decode(data)?;
        if !matches!(portable.schema_version, 1..=3)
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
        for (target, state) in &portable.learning {
            let version = state
                .correction
                .as_ref()
                .map(|correction| correction.answer_version)
                .or_else(|| state.game_review.map(|legacy| legacy.answer_version));
            if version.is_some_and(|version| {
                portable
                    .catalog
                    .items
                    .get(target)
                    .is_none_or(|item| version != item.version)
            }) {
                return Err(AppError::new(
                    "invalid_learning",
                    "Game review refers to a different answer version.",
                ));
            }
            self.components
                .learning
                .validate_state(state)
                .map_err(|e| domain("invalid_learning", e))?;
        }
        for key in &portable.game_failures {
            if key.source.is_empty()
                || key.game_id.is_empty()
                || portable
                    .catalog
                    .items
                    .get(&key.target)
                    .is_none_or(|item| key.version.0 == 0 || item.version.0 < key.version.0)
            {
                return Err(AppError::new(
                    "invalid_learning",
                    "Invalid game failure provenance.",
                ));
            }
        }
        self.store.restore_with_game_failures(
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
            &portable.game_failures,
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
