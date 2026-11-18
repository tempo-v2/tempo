//! Preview user-supplied legal PGN segments and accept an explicitly selected batch.
use super::{
    Tempo,
    content_preview::{check_dataset, preview_id, read_preview, source_id, stale_preview},
};
use crate::{
    AppError, app_error::domain, commands::TacticCaptureCommand, views::StudyView,
    wire::STUDY_WIRE_VERSION,
};
use serde_json::{Value, json};
use tempo_chess::{ChangeKind, plan_tactic_capture, preview_tactic_pgn};
use tempo_learning::LearningState;
use tempo_store::{ContentReceipt, ContentSource, Write};

impl Tempo {
    pub fn handle_tactic_capture(&mut self, command: TacticCaptureCommand) -> Value {
        self.tactic_capture(command)
            .unwrap_or_else(|error| json!(StudyView::from(error)))
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
                    json!({ "schema_version": STUDY_WIRE_VERSION, "status": "tactics_preview",
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
                let plan = plan_tactic_capture(&snapshot.catalog, &catalog)
                    .map_err(|error| domain("content_change_error", error))?;
                let writes = plan
                    .content
                    .changes
                    .iter()
                    .filter(|entry| entry.kind == ChangeKind::Added)
                    .map(|entry| Write::Learning {
                        target: entry.target.clone(),
                        state: LearningState::default(),
                    })
                    .collect();
                let reply = json!({ "schema_version": STUDY_WIRE_VERSION, "status": "tactics_saved",
                    "count": selected.len(), "alternate_solutions": plan.alternate_solutions });
                let payload = reply.to_string();
                self.commit_catalog(
                    &snapshot,
                    &plan.content.catalog,
                    writes,
                    Some(ContentSource {
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
