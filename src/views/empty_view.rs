use serde::{Deserialize, Serialize};
use tempo_types::DatasetId;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmptyView {
    pub schema_version: u32,
    pub status: String,
    pub dataset_id: DatasetId,
}
