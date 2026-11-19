//! Bounded original PGN data travels with both preview and save.
use serde::{Deserialize, Serialize};
use tempo_chess::LearnerColor;
use tempo_types::{DatasetId, Revision};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepertoireInput {
    pub dataset_id: DatasetId,
    pub pgn: String,
    pub name: String,
    pub learner_color: LearnerColor,
    #[serde(default)]
    pub repertoire_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum RepertoireCommand {
    List,
    ReadDocument {
        dataset_id: DatasetId,
        repertoire_id: String,
    },
    EditPreview {
        base_revision: Revision,
        #[serde(flatten)]
        input: RepertoireInput,
    },
    MarkedPreview {
        dataset_id: DatasetId,
        repertoire_id: Option<String>,
        base_revision: Option<Revision>,
        name: String,
        learner_color: LearnerColor,
        anchor_fen: String,
        anchor_route: Option<Vec<String>>,
        lines: Vec<Vec<String>>,
    },
    Preview(RepertoireInput),
    Save {
        #[serde(flatten)]
        input: RepertoireInput,
        token: String,
    },
    Rename {
        dataset_id: DatasetId,
        repertoire_id: String,
        name: String,
    },
    ResetProgress {
        dataset_id: DatasetId,
        repertoire_id: String,
        command_id: String,
    },
}
