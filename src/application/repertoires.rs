//! Preview and replace one complete repertoire, fenced independently of study reviews.
use super::{
    Tempo,
    content_preview::{
        check_dataset, preview_id, read_preview, signature, source_id, stale_preview,
    },
};
use crate::{
    AppError,
    app_error::domain,
    commands::{RepertoireCommand, RepertoireInput},
    views::ErrorView,
    wire::CONTENT_WIRE_VERSION,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempo_chess::{
    Catalog, ChangeKind, ContentItem, ContentKind, ContentPlan, LearnerColor, compile_repertoire,
    plan_repertoire_replacement,
};
use tempo_learning::{DailyLearning, LearningState};
use tempo_store::{ContentReceipt, ContentSource, RepertoireReset, Snapshot, Write};
use tempo_types::DatasetId;

#[derive(Serialize)]
struct Summary {
    id: String,
    name: String,
    learner_color: LearnerColor,
    positions: usize,
}

impl Tempo {
    /// Handle local content operations against the same durable learning dataset.
    pub fn handle_repertoire(&mut self, command: RepertoireCommand) -> Value {
        self.repertoire(command)
            .unwrap_or_else(|error| json!(ErrorView::for_content(error)))
    }

    fn repertoire(&mut self, command: RepertoireCommand) -> Result<Value, AppError> {
        let key = self
            .daily
            .as_ref()
            .ok_or_else(|| AppError::new("unsupported", "Use the local study release."))?
            .preview_key;
        if let RepertoireCommand::ReadDocument {
            dataset_id,
            repertoire_id,
        } = &command
        {
            let document = self.store.repertoire_document(repertoire_id)?;
            if &document.dataset != dataset_id {
                return Err(super::content_preview::stale_dataset());
            }
            let item = document.item.ok_or_else(unknown_repertoire)?;
            let availability = match document.pgn {
                Some(pgn) => json!({ "status": "available", "pgn": pgn }),
                None => json!({ "status": "unavailable" }),
            };
            return Ok(json!({
                "schema_version": CONTENT_WIRE_VERSION, "status": "document",
                "dataset_id": document.dataset, "repertoire_id": repertoire_id,
                "name": item.label, "learner_color": learner_color(&item),
                "revision": document.revision, "document": availability,
            }));
        }
        if let RepertoireCommand::ResetProgress {
            dataset_id,
            repertoire_id,
            command_id,
        } = &command
        {
            return self.reset_repertoire(dataset_id, repertoire_id, command_id);
        }
        let (snapshot, revision) = self.store.content_state()?;
        match command {
            RepertoireCommand::ReadDocument { .. } | RepertoireCommand::ResetProgress { .. } => {
                unreachable!("handled without reading the catalog")
            }
            RepertoireCommand::List => Ok(list(&snapshot.dataset, &snapshot.catalog)),
            RepertoireCommand::Rename {
                dataset_id,
                repertoire_id,
                name,
            } => {
                check_dataset(&snapshot, &dataset_id)?;
                let name = validate_name(&name)?;
                let mut catalog = snapshot.catalog.clone();
                let mut found = false;
                for item in catalog.items.values_mut().filter(|item| {
                    matches!(&item.definition.kind, ContentKind::Repertoire { repertoire, .. } if repertoire == &repertoire_id)
                }) {
                    found = true;
                    item.label = name.to_owned();
                }
                if !found {
                    return Err(unknown_repertoire());
                }
                if catalog != snapshot.catalog {
                    self.commit_catalog(&snapshot, &catalog, vec![], None, None)?;
                }
                Ok(list(&snapshot.dataset, &catalog))
            }
            RepertoireCommand::Preview(input) => {
                let id = input.repertoire_id.clone().unwrap_or_else(|| {
                    let digest = signature(&key, revision, "", &input);
                    format!("repertoire-{}", &digest[..32])
                });
                let token = document_preview(&key, revision, &id, &input);
                let (_, incoming, plan) = prepare(&snapshot, &input, &id)?;
                Ok(preview(&snapshot, &incoming, &plan, token))
            }
            RepertoireCommand::EditPreview {
                base_revision,
                input,
            } => {
                check_dataset(&snapshot, &input.dataset_id)?;
                if base_revision != revision {
                    return Err(stale_draft());
                }
                let id = input
                    .repertoire_id
                    .as_deref()
                    .ok_or_else(unknown_repertoire)?;
                let document = self.store.repertoire_document(id)?;
                if document.dataset != input.dataset_id || document.revision != base_revision {
                    return Err(stale_draft());
                }
                if document.item.is_none() {
                    return Err(unknown_repertoire());
                }
                if document.pgn.is_none() {
                    return Err(AppError::new(
                        "document_unavailable",
                        "Upload a complete replacement to establish the current PGN.",
                    ));
                }
                let (_, incoming, plan) = prepare(&snapshot, &input, id)?;
                let token = document_preview(&key, revision, id, &input);
                Ok(preview(&snapshot, &incoming, &plan, token))
            }
            RepertoireCommand::Save { input, token } => {
                let (revision_in_token, id) =
                    if let Some(versioned) = token.strip_prefix(DOCUMENT_PREVIEW) {
                        read_preview(&key, versioned, &(DOCUMENT_PREVIEW, &input))?
                    } else {
                        read_preview(&key, &token, &input)?
                    };
                check_dataset(&snapshot, &input.dataset_id)?;
                if let Some(saved) = self.store.content_receipt(&token)? {
                    return serde_json::from_str(&saved).map_err(|e| domain("invalid_receipt", e));
                }
                if revision != revision_in_token {
                    return Err(stale_preview());
                }
                let (source, _, plan) = prepare(&snapshot, &input, id)?;
                let reply = list(&snapshot.dataset, &plan.catalog);
                let encoded = reply.to_string();
                let receipt = ContentReceipt {
                    request: &token,
                    reply: &encoded,
                };
                let learning = plan
                    .changes
                    .iter()
                    .filter_map(|change| {
                        let state = match change.kind {
                            ChangeKind::Added => LearningState::default(),
                            ChangeKind::Semantic => {
                                DailyLearning::changed_answer(&snapshot.learning[&change.target])
                            }
                            _ => return None,
                        };
                        Some(Write::Learning {
                            target: change.target.clone(),
                            state,
                        })
                    })
                    .collect();
                self.commit_catalog(
                    &snapshot,
                    &plan.catalog,
                    learning,
                    Some(ContentSource::RepertoireDocument {
                        id: &source,
                        pgn: &input.pgn,
                        repertoire_id: id,
                    }),
                    Some(receipt),
                )?;
                Ok(reply)
            }
        }
    }

    fn reset_repertoire(
        &mut self,
        dataset: &DatasetId,
        repertoire: &str,
        command_id: &str,
    ) -> Result<Value, AppError> {
        if command_id.trim().is_empty() || command_id.len() > 120 {
            return Err(AppError::new(
                "invalid_command_id",
                "A reset command identity is required.",
            ));
        }
        let at = self.components.clock.now();
        match self.store.reset_repertoire(
            dataset,
            repertoire,
            command_id,
            at.unix_ms,
            &LearningState::default(),
        )? {
            RepertoireReset::Applied(targets) => {
                let daily = self.daily.as_mut().expect("daily release");
                for target in &targets {
                    daily.queue.clear_deferral(target);
                }
            }
            RepertoireReset::Replayed => {}
            RepertoireReset::UnknownRepertoire => return Err(unknown_repertoire()),
            RepertoireReset::StaleDataset => return Err(super::content_preview::stale_dataset()),
            RepertoireReset::CommandConflict => {
                return Err(AppError::new(
                    "command_conflict",
                    "This reset command was already used for another repertoire.",
                ));
            }
        }
        Ok(json!({
            "schema_version": CONTENT_WIRE_VERSION, "status": "progress_reset",
            "dataset_id": dataset, "repertoire_id": repertoire,
        }))
    }
}

// Separate new Preview authority from any legacy receipt-only Save issued at
// the same content revision. The version is bound into the MAC.
const DOCUMENT_PREVIEW: &str = "document-v1:";

fn document_preview(
    key: &[u8; 32],
    revision: tempo_types::Revision,
    id: &str,
    input: &RepertoireInput,
) -> String {
    let signed = preview_id(key, revision, id, &(DOCUMENT_PREVIEW, input));
    format!("{DOCUMENT_PREVIEW}{signed}")
}

fn stale_draft() -> AppError {
    AppError::new(
        "stale_draft",
        "The repertoire changed after you opened it. Reload the current PGN before previewing again.",
    )
}

fn unknown_repertoire() -> AppError {
    AppError::new("unknown_repertoire", "Choose an existing repertoire.")
}

fn validate_name(name: &str) -> Result<&str, AppError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 || name.chars().any(char::is_control) {
        return Err(AppError::new(
            "invalid_name",
            "Enter a repertoire name of 1–120 characters.",
        ));
    }
    Ok(name)
}

fn prepare(
    snapshot: &Snapshot,
    input: &RepertoireInput,
    id: &str,
) -> Result<(String, Catalog, ContentPlan), AppError> {
    check_dataset(snapshot, &input.dataset_id)?;
    let name = validate_name(&input.name)?;
    if let Some(replacement) = &input.repertoire_id {
        let metadata = summaries(&snapshot.catalog)
            .remove(replacement)
            .ok_or_else(unknown_repertoire)?;
        if id != replacement
            || input.learner_color != metadata.learner_color
            || name != metadata.name
        {
            return Err(AppError::new(
                "changed_destination",
                "Replacement keeps the name and learner color.",
            ));
        }
    }
    let source = source_id(&input.pgn);
    let incoming = compile_repertoire(&input.pgn, id, name, input.learner_color, &source)
        .map_err(|e| domain("invalid_pgn", e))?;
    let plan = plan_repertoire_replacement(&snapshot.catalog, &incoming, id)
        .map_err(|e| domain("content_change_error", e))?;
    Ok((source, incoming, plan))
}

fn summaries(catalog: &Catalog) -> BTreeMap<String, Summary> {
    let mut summaries = BTreeMap::new();
    for item in catalog.items.values() {
        let ContentKind::Repertoire { repertoire, .. } = &item.definition.kind else {
            continue;
        };
        let summary = summaries
            .entry(repertoire.clone())
            .or_insert_with(|| Summary {
                id: repertoire.clone(),
                name: item.label.clone(),
                learner_color: learner_color(item),
                positions: 0,
            });
        summary.positions += usize::from(item.active());
    }
    summaries
}

pub(super) fn learner_color(item: &ContentItem) -> LearnerColor {
    if item.definition.decisions[0].fen.split_whitespace().nth(1) == Some("b") {
        LearnerColor::Black
    } else {
        LearnerColor::White
    }
}

fn list(dataset: &DatasetId, catalog: &Catalog) -> Value {
    json!({
        "schema_version": CONTENT_WIRE_VERSION, "status": "repertoires", "dataset_id": dataset,
        "repertoires": summaries(catalog).into_values().collect::<Vec<_>>(),
    })
}

fn preview(snapshot: &Snapshot, incoming: &Catalog, plan: &ContentPlan, token: String) -> Value {
    let mut added = 0;
    let mut removed = 0;
    let mut changed = 0;
    let mut affected_learning = 0;
    for change in &plan.changes {
        let active_before = snapshot
            .catalog
            .items
            .get(&change.target)
            .is_some_and(|item| item.active());
        let active_after = plan.catalog.items[&change.target].active();
        added += usize::from(!active_before && active_after);
        removed += usize::from(active_before && !active_after);
        changed += usize::from(change.kind == ChangeKind::Semantic);
        if (active_before && !active_after || change.kind == ChangeKind::Semantic)
            && snapshot
                .learning
                .get(&change.target)
                .is_some_and(|state| state.first_introduced.is_some())
        {
            affected_learning += 1;
        }
    }
    json!({
        "schema_version": CONTENT_WIRE_VERSION, "status": "preview", "token": token,
        "positions": incoming.items.len(), "routes": incoming.lines.len(),
        "added": added, "removed": removed, "changed": changed, "affected_learning": affected_learning,
    })
}
