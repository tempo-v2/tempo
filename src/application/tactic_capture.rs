//! Preview user-supplied legal PGN segments and accept an explicitly selected batch.
use super::{
    Tempo,
    content_preview::{check_dataset, preview_id, read_preview, source_id, stale_preview},
};
use crate::{
    AppError, app_error::domain, commands::TacticCaptureCommand, views::ErrorView,
    wire::CONTENT_WIRE_VERSION,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tempo_chess::{plan_tactic_capture, preview_tactic_pgn};
use tempo_learning::DailyLearning;

pub(super) const PERSONAL_CAPTURE: &str = "tempo:capture-save";
use tempo_store::{ContentReceipt, ContentSource, Write};

impl Tempo {
    pub fn handle_tactic_capture(&mut self, command: TacticCaptureCommand) -> Value {
        self.tactic_capture(command)
            .unwrap_or_else(|error| json!(ErrorView::for_content(error)))
    }

    fn tactic_capture(&mut self, command: TacticCaptureCommand) -> Result<Value, AppError> {
        let key = self
            .daily
            .as_ref()
            .ok_or_else(|| AppError::new("unsupported", "Use the local study release."))?
            .preview_key;
        let (snapshot, revision) = self.store.content_state()?;
        match command {
            TacticCaptureCommand::Preview(input) => {
                check_dataset(&snapshot, &input.dataset_id)?;
                let source = source_id(&input.pgn);
                let preview = preview_tactic_pgn(&input.pgn, &input.selections, &source)
                    .map_err(|error| domain("invalid_pgn", error))?;
                let token = preview_id(&key, revision, "tactics", &input);
                Ok(
                    json!({ "schema_version": CONTENT_WIRE_VERSION, "status": "tactics_preview",
                    "token": token, "rows": preview.rows }),
                )
            }
            TacticCaptureCommand::Save {
                input,
                token,
                mut selected,
            } => {
                check_dataset(&snapshot, &input.dataset_id)?;
                let (seen_revision, purpose) = read_preview(&key, &token, &input)?;
                if purpose != "tactics" {
                    return Err(stale_preview());
                }
                selected.sort_unstable();
                let request_bytes = serde_json::to_vec(&(&token, &selected))
                    .expect("serializable tactic selection");
                let receipt_id = format!("tactics:{}", blake3::hash(&request_bytes).to_hex());
                if let Some(saved) = self.store.content_receipt(&receipt_id)? {
                    return serde_json::from_str(&saved)
                        .map_err(|error| domain("invalid_receipt", error));
                }
                if seen_revision != revision {
                    return Err(stale_preview());
                }
                let source = source_id(&input.pgn);
                let preview = preview_tactic_pgn(&input.pgn, &input.selections, &source)
                    .map_err(|error| domain("invalid_pgn", error))?;
                let catalog = preview
                    .selected_catalog(&selected)
                    .map_err(|error| domain("invalid_selection", error))?;
                let mut plan = plan_tactic_capture(&snapshot.catalog, &catalog)
                    .map_err(|error| domain("content_change_error", error))?;
                let at = self.components.clock.now();
                let targets: BTreeSet<_> = plan
                    .content
                    .changes
                    .iter()
                    .map(|entry| entry.target.clone())
                    .collect();
                let mut writes = Vec::new();
                for target in targets {
                    let item = plan
                        .content
                        .catalog
                        .items
                        .get_mut(&target)
                        .expect("planned target");
                    let prior = snapshot.learning.get(&target).cloned().unwrap_or_default();
                    if item.active() && !prior.introduced {
                        let state = DailyLearning::introduce_capture(&prior, at)
                            .map_err(|error| domain("learning_error", error))?;
                        if !item
                            .provenance
                            .iter()
                            .any(|value| value == PERSONAL_CAPTURE)
                        {
                            item.provenance.push(PERSONAL_CAPTURE.into());
                            item.provenance.sort();
                        }
                        writes.push(Write::Learning { target, state });
                    }
                }
                let reply = json!({ "schema_version": CONTENT_WIRE_VERSION, "status": "tactics_saved",
                    "count": selected.len(), "ready_for_review": writes.len(), "alternate_solutions": plan.alternate_solutions });
                let payload = reply.to_string();
                self.commit_catalog(
                    &snapshot,
                    &plan.content.catalog,
                    writes,
                    Some(ContentSource::Archive {
                        id: &source,
                        pgn: &input.pgn,
                    }),
                    Some(ContentReceipt {
                        request: &receipt_id,
                        reply: &payload,
                    }),
                )?;
                Ok(reply)
            }
        }
    }
}
