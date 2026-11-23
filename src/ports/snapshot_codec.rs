//! Translate portable study data into an interchange representation.
use crate::{AppError, PortableSnapshot};

/// Translate portable study data into an interchange representation.
///
/// Supply this port to [`crate::Tempo::export`] or [`crate::Tempo::restore`].
/// A codec handles encoding; Tempo still owns schema, policy and destination
/// checks. Codecs do not fetch games or write the application store.
pub trait SnapshotCodec {
    fn encode(&self, snapshot: &PortableSnapshot) -> Result<String, AppError>;
    fn decode(&self, data: &str) -> Result<PortableSnapshot, AppError>;
}

#[cfg(test)]
mod tests {
    use super::SnapshotCodec;
    use crate::{AppError, JsonSnapshotCodec, PortableSnapshot};

    // A small alternate representation illustrates replacing the codec, not Tempo.
    struct TaggedJson;
    impl SnapshotCodec for TaggedJson {
        fn encode(&self, snapshot: &PortableSnapshot) -> Result<String, AppError> {
            Ok(format!(
                "tempo-export\n{}",
                JsonSnapshotCodec.encode(snapshot)?
            ))
        }
        fn decode(&self, data: &str) -> Result<PortableSnapshot, AppError> {
            let body = data
                .strip_prefix("tempo-export\n")
                .ok_or_else(|| AppError::new("codec_error", "missing header"))?;
            JsonSnapshotCodec.decode(body)
        }
    }

    #[test]
    fn application_export_accepts_an_alternate_codec_through_the_same_port() {
        let (app, _directory) = crate::components::test_study();
        let codec: &dyn SnapshotCodec = &TaggedJson;
        let data = app.export(codec).unwrap();
        assert!(data.starts_with("tempo-export\n"));
        assert_eq!(
            codec.decode(&data).unwrap().catalog,
            app.snapshot().unwrap().catalog
        );
    }
}
