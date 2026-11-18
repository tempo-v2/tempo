//! An exhausted queue for the current moment, distinct from an application error.
use serde::{Deserialize, Serialize};
use tempo_types::DatasetId;

/// Tell the UI there is currently no eligible exercise in this dataset.
///
/// `Tempo::handle(StudyCommand::Next)` produces this after Queue returns no item.
/// The UI can display an empty state; a later Next may succeed as facts change.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmptyView {
    pub schema_version: u32,
    pub status: String,
    pub dataset_id: DatasetId,
    pub done: bool,
    pub outstanding: usize,
    pub repertoire_remaining: usize,
    pub tactics_remaining: usize,
    #[serde(default)]
    pub can_practice_tactics: bool,
    #[serde(default)]
    pub can_learn_more_tactics: bool,
    #[serde(default)]
    pub learn_more_count: usize,
}

#[cfg(test)]
mod tests {
    use crate::{
        Components, Tempo,
        wire::{StudyCommand, StudyView},
    };
    use tempo_chess::Catalog;
    use tempo_store::MemoryStore;

    #[test]
    fn asking_an_empty_dataset_returns_an_empty_view_without_fabricating_an_attempt() {
        let components = Components::fixtures();
        let store = MemoryStore::new(
            "empty".into(),
            Catalog::default(),
            components.learning.settings(),
        )
        .unwrap();
        let mut app = Tempo::new(Box::new(store), components).unwrap();
        let StudyView::Empty(view) = app.handle(StudyCommand::Next) else {
            panic!("no content means no eligible exercise");
        };
        assert_eq!(
            serde_json::to_value(view).unwrap(),
            serde_json::json!({
                "schema_version": 4, "status": "empty", "dataset_id": "empty",
                "done": true, "outstanding": 0, "repertoire_remaining": 0, "tactics_remaining": 0,
                "can_practice_tactics": false, "can_learn_more_tactics": false, "learn_more_count": 0
            })
        );
        assert!(app.snapshot().unwrap().attempts.is_empty());
    }
}
