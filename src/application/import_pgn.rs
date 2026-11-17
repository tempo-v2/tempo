//! Parse content and commit its semantic change plan with affected learning state.
use super::Tempo;
use crate::{AppError, app_error::domain};
use tempo_chess::{ChangeKind, ImportRequest};
use tempo_learning::LearningState;
use tempo_store::Write;
use tempo_types::DatasetId;

impl Tempo {
    /// Import content into this dataset through the selected parser.
    ///
    /// Metadata-only changes retain learning. New or semantically changed targets
    /// start with default learning; an attempt pointing at changed content is no
    /// longer active. Portable restore is a separate operation.
    pub fn import_pgn(
        &mut self,
        dataset: &DatasetId,
        request: &ImportRequest,
    ) -> Result<Vec<tempo_chess::ContentChange>, AppError> {
        let snapshot = self.store.snapshot()?;
        if dataset != &snapshot.dataset {
            return Err(AppError::new(
                "stale_dataset",
                "import belongs to a different dataset",
            ));
        }
        let parsed = self
            .components
            .parser
            .parse(request)
            .map_err(|e| domain("content_parse_error", e))?;
        let plan = tempo_chess::plan_import(&snapshot.catalog, &parsed)
            .map_err(|e| domain("content_change_error", e))?;
        let mut writes = vec![Write::Catalog(plan.catalog.clone())];
        for change in &plan.changes {
            if matches!(change.kind, ChangeKind::Added | ChangeKind::Semantic) {
                writes.push(Write::Learning {
                    target: change.target.clone(),
                    state: LearningState::default(),
                });
            }
        }
        if let Some(id) = &snapshot.active_attempt {
            let old = &snapshot.attempts[id].attempt;
            if plan
                .catalog
                .items
                .get(&old.target)
                .is_none_or(|item| !item.active() || item.version != old.content_version)
            {
                writes.push(Write::Active(None));
            }
        }
        self.commit(&snapshot, writes)?;
        self.remaining_block.clear();
        Ok(plan.changes)
    }
}

#[cfg(test)]
mod tests {
    use crate::Tempo;
    use tempo_chess::{ImportRequest, REPERTOIRE_PGN};

    #[test]
    fn importing_unchanged_repertoire_preserves_the_active_attempt() {
        let mut app = Tempo::demo().unwrap();
        let issued = app.next_exercise().unwrap().unwrap();
        let before = app.snapshot().unwrap();
        app.import_pgn(
            &before.dataset,
            &ImportRequest {
                pgn: REPERTOIRE_PGN.into(),
                source: "example-import".into(),
                repertoire: None,
            },
        )
        .unwrap();
        assert_eq!(
            app.resume(&issued.attempt.id).unwrap().attempt,
            issued.attempt
        );
        assert_eq!(app.snapshot().unwrap().learning, before.learning);
    }
}
