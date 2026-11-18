//! Schedule historical personal captures without inventing review history.
use super::Tempo;
use crate::{AppError, app_error::domain};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tempo_learning::DailyLearning;
use tempo_store::{Commit, ContentReceipt, Write};

impl Tempo {
    /// Preview or commit the one-time personal-capture backfill for this dataset.
    pub fn backfill_personal_tactics(
        &mut self,
        saved_sources: &BTreeSet<String>,
        apply: bool,
    ) -> Result<Value, AppError> {
        if self.daily.is_none() {
            return Err(AppError::new("unsupported", "Use the local study release."));
        }
        let snapshot = self.store.snapshot()?;
        let request = format!("personal-tactics-due-v1:{}", snapshot.dataset.0);
        if let Some(saved) = self.store.content_receipt(&request)? {
            let receipt: Value =
                serde_json::from_str(&saved).map_err(|error| domain("invalid_receipt", error))?;
            return Ok(json!({ "status": "already_applied", "receipt": receipt }));
        }

        let at = self.components.clock.now();
        let mut matched = 0;
        let mut introduced = 0;
        let mut brought_forward = 0;
        let mut writes = Vec::new();
        for item in snapshot.catalog.items.values().filter(|item| {
            item.active()
                && !item.definition.is_repertoire()
                && item.provenance.iter().any(|source| {
                    source
                        .strip_prefix("tempo1:capture:")
                        .is_some_and(|id| !id.is_empty())
                        || (source.starts_with("pgn:") && saved_sources.contains(source))
                })
        }) {
            matched += 1;
            let prior = &snapshot.learning[&item.target];
            let state = if !prior.introduced {
                introduced += 1;
                DailyLearning::introduce_capture(prior, at)
                    .map_err(|error| domain("learning_error", error))?
            } else if prior.due.is_some_and(|due| due > at.day) {
                brought_forward += 1;
                let mut state = prior.clone();
                state.due = Some(at.day);
                state
            } else {
                continue;
            };
            writes.push(Write::Learning {
                target: item.target.clone(),
                state,
            });
        }
        let report = json!({
            "status": if apply { "applied" } else { "dry_run" },
            "dataset_id": snapshot.dataset,
            "study_day": at.day,
            "matched": matched,
            "introduced": introduced,
            "brought_forward": brought_forward,
        });
        // An empty library has nothing to backfill and may receive its import later.
        if apply && matched > 0 {
            self.store.commit_content(
                Commit {
                    expected_dataset: snapshot.dataset,
                    expected_revision: snapshot.revision,
                    writes,
                },
                None,
                Some(ContentReceipt {
                    request: &request,
                    reply: &report.to_string(),
                }),
            )?;
        }
        Ok(report)
    }
}
