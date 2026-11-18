//! Original PGN and selected interpretations are kept with the pending save.
use serde::{Deserialize, Serialize};
use tempo_chess::TacticPgnSelection;
use tempo_types::DatasetId;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureInput {
    pub dataset_id: DatasetId,
    pub pgn: String,
    #[serde(default)]
    pub selections: Vec<TacticPgnSelection>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum TacticCaptureCommand {
    Preview(CaptureInput),
    Save {
        #[serde(flatten)]
        input: CaptureInput,
        token: String,
        selected: Vec<usize>,
    },
}
