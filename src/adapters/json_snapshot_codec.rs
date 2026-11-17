//! JSON interchange for the portable content and learning snapshot.
use crate::{AppError, PortableSnapshot, SnapshotCodec, app_error::domain};

/// JSON interchange for the portable content and learning snapshot.
///
/// Supply this codec to export/restore for executable examples. An enriched-PGN
/// codec can implement the same port without changing those application commands.
/// Decoding alone does not check whether a snapshot can be restored.
pub struct JsonSnapshotCodec;
impl SnapshotCodec for JsonSnapshotCodec {
    fn encode(&self, snapshot: &PortableSnapshot) -> Result<String, AppError> {
        serde_json::to_string_pretty(snapshot).map_err(|e| domain("codec_error", e))
    }
    fn decode(&self, data: &str) -> Result<PortableSnapshot, AppError> {
        serde_json::from_str(data).map_err(|e| domain("codec_error", e))
    }
}

#[cfg(test)]
mod tests {
    use super::JsonSnapshotCodec;
    use crate::{SnapshotCodec, Tempo};

    #[test]
    fn the_json_adapter_round_trips_export_data_and_reports_malformed_input() {
        let app = Tempo::demo().unwrap();
        let encoded = app.export(&JsonSnapshotCodec).unwrap();
        let portable = JsonSnapshotCodec.decode(&encoded).unwrap();
        assert_eq!(portable.catalog, app.snapshot().unwrap().catalog);
        assert_eq!(
            JsonSnapshotCodec.decode("not json").unwrap_err().code,
            "codec_error"
        );
    }
}
