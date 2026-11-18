//! Read a portable content/learning snapshot through a caller-selected codec.
use super::Tempo;
use crate::{AppError, PortableSnapshot, SnapshotCodec};

impl Tempo {
    /// Encode catalog, learning and policy settings without changing the store.
    ///
    /// Attempts and receipts are intentionally absent from this portable format;
    /// callers pass the encoded payload to a fresh destination's `restore` method.
    pub fn export(&self, codec: &dyn SnapshotCodec) -> Result<String, AppError> {
        let snapshot = self.store.snapshot()?;
        codec.encode(&PortableSnapshot {
            schema_version: if self.daily.is_some() { 2 } else { 1 },
            catalog: snapshot.catalog,
            learning: snapshot.learning,
            policy: snapshot.policy,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{JsonSnapshotCodec, SnapshotCodec, Tempo};

    #[test]
    fn export_is_a_read_and_leaves_an_active_attempt_untouched() {
        let mut app = Tempo::demo().unwrap();
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
