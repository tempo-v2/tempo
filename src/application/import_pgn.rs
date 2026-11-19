//! Parse content and commit its semantic change plan with affected learning state.
use super::Tempo;
use crate::{AppError, app_error::domain};
use tempo_chess::{Catalog, ChangeKind, ContentPlan, ImportRequest};
use tempo_learning::LearningState;
use tempo_store::{ContentState, Write};
use tempo_types::DatasetId;

impl Tempo {
    /// Additively import content through the standard profile's selected parser.
    ///
    /// Omitted content and metadata-only learning are retained. New or
    /// semantically changed targets start with default learning. Daily Study
    /// accepts repertoire PGN through signed Preview and Save instead.
    pub fn import_pgn(
        &mut self,
        dataset: &DatasetId,
        request: &ImportRequest,
    ) -> Result<Vec<tempo_chess::ContentChange>, AppError> {
        let fixture = self.fixture.as_ref().ok_or_else(|| {
            AppError::new(
                "unsupported_capability",
                "Use Repertoires to preview and save repertoire PGN.",
            )
        })?;
        let snapshot = self.import_snapshot(dataset)?;
        let parsed = fixture
            .parser
            .parse(request)
            .map_err(|e| domain("content_parse_error", e))?;
        Ok(self.apply_catalog_to(&snapshot, &parsed)?.changes)
    }

    /// Apply validated content atomically. The standard profile uses additive
    /// import. Daily Study accepts only active tactics with no repertoire routes
    /// and uses the capture planner to preserve existing definitions and learning.
    pub fn apply_catalog(
        &mut self,
        dataset: &DatasetId,
        catalog: &Catalog,
    ) -> Result<ContentPlan, AppError> {
        let snapshot = self.import_snapshot(dataset)?;
        self.apply_catalog_to(&snapshot, catalog)
    }

    /// Add validated tactics while retaining existing targets byte for byte.
    /// The changes are in incoming target order, including matches to an
    /// existing target. The transfer report records aliases to preexisting
    /// content without modifying its metadata.
    pub fn apply_tactic_transfer(&mut self, catalog: &Catalog) -> Result<ContentPlan, AppError> {
        let snapshot = self.store.content_state()?;
        let mut plan = tempo_chess::plan_tactic_capture(&snapshot.catalog, catalog)
            .map_err(|error| domain("content_change_error", error))?
            .content;
        for change in &mut plan.changes {
            if let Some(existing) = snapshot.catalog.items.get(&change.target) {
                plan.catalog
                    .items
                    .insert(change.target.clone(), existing.clone());
                change.kind = ChangeKind::Unchanged;
            }
        }
        self.publish_import(&snapshot, plan)
    }

    fn import_snapshot(&self, dataset: &DatasetId) -> Result<ContentState, AppError> {
        let snapshot = self.store.content_state()?;
        if dataset != &snapshot.dataset {
            return Err(AppError::new(
                "stale_dataset",
                "import belongs to a different dataset",
            ));
        }
        Ok(snapshot)
    }

    fn apply_catalog_to(
        &mut self,
        snapshot: &ContentState,
        catalog: &Catalog,
    ) -> Result<ContentPlan, AppError> {
        let plan = if self.daily.is_some() {
            tempo_chess::plan_tactic_capture(&snapshot.catalog, catalog).map(|plan| plan.content)
        } else {
            tempo_chess::plan_import(&snapshot.catalog, catalog)
        }
        .map_err(|e| domain("content_change_error", e))?;
        self.publish_import(snapshot, plan)
    }

    fn publish_import(
        &mut self,
        snapshot: &ContentState,
        plan: ContentPlan,
    ) -> Result<ContentPlan, AppError> {
        if plan.catalog == snapshot.catalog {
            return Ok(plan);
        }
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
        self.commit(&snapshot.dataset, snapshot.revision, writes)?;
        self.remaining_block.clear();
        Ok(plan)
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
                learner_color: None,
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
