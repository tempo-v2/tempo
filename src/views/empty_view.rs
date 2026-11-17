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
                "schema_version": 2, "status": "empty", "dataset_id": "empty"
            })
        );
        assert!(app.snapshot().unwrap().attempts.is_empty());
    }
}
