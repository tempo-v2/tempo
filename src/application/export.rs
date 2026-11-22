//! Read a portable content/learning snapshot through a caller-selected codec.
use super::Tempo;
use crate::{AppError, PortableSnapshot, SnapshotCodec};

impl Tempo {
    /// Encode catalog, learning and policy settings without changing the store.
    ///
    /// Attempts and receipts are intentionally absent from this portable format;
    /// callers pass the encoded payload to a fresh destination's `restore` method.
    pub fn export(&self, codec: &dyn SnapshotCodec) -> Result<String, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.export.export");
        let (snapshot, game_evidence) = self.store.portable_state()?;
        codec.encode(&PortableSnapshot {
            schema_version: 5,
            game_evidence,
            catalog: snapshot.catalog,
            learning: snapshot.learning,
            policy: snapshot.policy,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{JsonSnapshotCodec, SnapshotCodec};

    #[test]
    fn export_is_a_read_and_leaves_an_active_attempt_untouched() {
        let (mut app, _directory) = crate::components::test_study();
        app.next_exercise().unwrap();
        let before = app.snapshot().unwrap();
        let portable = JsonSnapshotCodec
            .decode(&app.export(&JsonSnapshotCodec).unwrap())
            .unwrap();
        assert_eq!(portable.catalog, before.catalog);
        assert_eq!(portable.learning, before.learning);
        assert_eq!(app.snapshot().unwrap(), before);
    }
}
