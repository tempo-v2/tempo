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
        let _diagnostic = crate::diagnostics::Span::new("app.import_pgn.import_pgn");
        let fixture = self.fixture.as_ref().ok_or_else(|| {
            AppError::new(
                "unsupported",
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
        let _diagnostic = crate::diagnostics::Span::new("app.import_pgn.apply_catalog");
        let snapshot = self.import_snapshot(dataset)?;
        self.apply_catalog_to(&snapshot, catalog)
    }

    /// Add validated tactics while retaining existing target definitions and
    /// source attribution. Matching transfers can add or disambiguate the
    /// evidence for the preceding opponent move. Changes follow input order.
    pub fn apply_tactic_transfer(&mut self, catalog: &Catalog) -> Result<ContentPlan, AppError> {
        let _diagnostic = crate::diagnostics::Span::new("app.import_pgn.apply_tactic_transfer");
        let snapshot = self.store.content_state()?;
        let mut plan = tempo_chess::plan_tactic_capture(&snapshot.catalog, catalog)
            .map_err(|error| domain("content_change_error", error))?
            .content;
        for change in &mut plan.changes {
            if let Some(existing) = snapshot.catalog.items.get(&change.target) {
                let retained = plan
                    .catalog
                    .items
                    .get_mut(&change.target)
                    .expect("planned target");
                let lead_in = std::mem::take(&mut retained.opponent_lead_in);
                let game_capture = retained.game_capture.take();
                retained.clone_from(existing);
                change.kind = if lead_in == retained.opponent_lead_in
                    && game_capture == retained.game_capture
                {
                    ChangeKind::Unchanged
                } else {
                    retained.opponent_lead_in = lead_in;
                    retained.game_capture = game_capture;
                    ChangeKind::Metadata
                };
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
