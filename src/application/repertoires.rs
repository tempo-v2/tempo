//! Preview and replace one complete repertoire, fenced independently of study reviews.
use super::{
    Tempo,
    content_preview::{
        check_dataset, preview_id, read_preview, signature, source_id, stale_preview,
    },
};
use crate::{
    AppError,
    app_error::{domain, study_input},
    commands::{RepertoireCommand, RepertoireInput},
    game_model::model_input_stamp,
    views::ErrorView,
    wire::CONTENT_WIRE_VERSION,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tempo_chess::{
    AuthoredRepertoire, Catalog, ChangeKind, ContentItem, ContentKind, ContentPlan, LearnerColor,
    compile_repertoire, plan_repertoire_replacement,
};
use tempo_learning::{DailyLearning, LearningState};
use tempo_store::{
    ContentReceipt, ContentSource, ContentState, RepertoireActivity, RepertoireActivityCommand,
    RepertoireActivityRead, RepertoireActivityResult, RepertoireReset, Write, model_targets,
};
use tempo_types::{DatasetId, Revision};

#[derive(Serialize)]
struct Summary {
    id: String,
    name: String,
    learner_color: LearnerColor,
    positions: usize,
    activity: RepertoireActivity,
}

impl Tempo {
    /// Handle local content operations against the same durable learning dataset.
    pub fn handle_repertoire(&mut self, command: RepertoireCommand) -> Value {
        let _diagnostic = crate::diagnostics::Span::new("app.repertoires.handle_repertoire");
        self.repertoire(command)
            .unwrap_or_else(|error| json!(ErrorView::for_content(error)))
    }

    /// Authorize an append-only gap draft against the same content snapshot
    /// that the signed Save will commit.
    pub(super) fn gap_response_token(
        &self,
        input: &RepertoireInput,
        id: &str,
        base_revision: Revision,
    ) -> Result<String, AppError> {
        let key = self
            .daily
            .as_ref()
            .ok_or_else(|| AppError::new("unsupported", "Use the local study release."))?
            .preview_key;
        let snapshot = self.store.content_state()?;
        let revision = snapshot.content_revision;
        check_dataset(&snapshot, &input.dataset_id)?;
        if revision != base_revision {
            return Err(stale_draft());
        }
        if snapshot.suspended_repertoires.contains(id) {
            return Err(suspended_repertoire());
        }
        let (_, _, plan) = prepare(&snapshot, input, id)?;
        if plan.changes.iter().any(|change| {
            change.kind == ChangeKind::Semantic
                || (snapshot
                    .catalog
                    .items
                    .get(&change.target)
                    .is_some_and(ContentItem::active)
                    && !plan.catalog.items[&change.target].active())
        }) {
            return Err(AppError::new(
                "unsafe_gap_response",
                "This response would change existing repertoire positions or learning.",
            ));
        }
        Ok(document_preview(&key, revision, id, input))
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
                "name": item.label, "learner_color": item.definition.learner_color().map_err(|error| domain("invalid_content", error))?,
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
        if let RepertoireCommand::SetActivity {
            dataset_id,
            repertoire_id,
            command_id,
            activity,
        } = &command
        {
            return self.set_repertoire_activity(dataset_id, repertoire_id, command_id, *activity);
        }
        let snapshot = self.store.content_state()?;
        let revision = snapshot.content_revision;
        let suspended = &snapshot.suspended_repertoires;
        match command {
            RepertoireCommand::ReadDocument { .. }
            | RepertoireCommand::ResetProgress { .. }
            | RepertoireCommand::SetActivity { .. } => {
                unreachable!("handled without reading the catalog")
            }
            RepertoireCommand::List => list(&snapshot.dataset, &snapshot.catalog, suspended),
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
                list(&snapshot.dataset, &catalog, suspended)
            }
            RepertoireCommand::MarkedPreview {
                dataset_id,
                repertoire_id,
                base_revision,
                name,
                learner_color,
                anchor_fen,
                anchor_route,
                lines,
            } => {
                check_dataset(&snapshot, &dataset_id)?;
                let source = if let Some(id) = repertoire_id.as_ref() {
                    let document = self.store.repertoire_document(id)?;
                    if document.dataset != dataset_id
                        || Some(document.revision) != base_revision
                        || document.revision != revision
                    {
                        return Err(stale_draft());
                    }
                    Some(document.pgn.ok_or_else(|| {
                        AppError::new(
                            "document_unavailable",
                            "This repertoire needs a verified source PGN.",
                        )
                    })?)
                } else {
                    None
                };
                let draft = tempo_chess::draft_marked_lines(
                    source.as_deref(),
                    learner_color,
                    &anchor_fen,
                    anchor_route.as_deref(),
                    &lines,
                )
                .map_err(|e| domain("invalid_marked_lines", e))?;
                let input = RepertoireInput {
                    dataset_id,
                    repertoire_id,
                    name,
                    learner_color,
                    pgn: draft.pgn,
                };
                let id = input.repertoire_id.clone().unwrap_or_else(|| {
                    let digest = signature(&key, revision, "", &input);
                    format!("repertoire-{}", &digest[..32])
                });
                let (_, incoming, plan) = prepare(&snapshot, &input, &id)?;
                let token = document_preview(&key, revision, &id, &input);
                let mut reply = preview(&snapshot, &incoming, &plan, token.clone());
                let mut save = json!(input);
                save["command"] = json!("save");
                save["token"] = json!(token);
                reply["save_command"] = save;
                reply["repertoire_id"] = json!(id);
                reply["replacements"] = json!(draft.replacements);
                reply["retired_routes"] = json!(draft.retired_routes);
                Ok(reply)
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
                let authored = AuthoredRepertoire::parse(&input.pgn)
                    .map_err(|error| domain("invalid_pgn", error))?;
                let model_input = model_input_stamp(
                    &authored,
                    input.learner_color,
                    model_targets(&plan.catalog, id),
                );
                let reply = list(&snapshot.dataset, &plan.catalog, suspended)?;
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
                                crate::diagnostics::measure("learning.changed_answer", || {
                                    DailyLearning::changed_answer(
                                        &snapshot.learning[&change.target],
                                    )
                                })
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
                        expected_content_revision: revision_in_token,
                        model_input: &model_input,
                    }),
                    Some(receipt),
                )?;
                Ok(reply)
            }
        }
    }

    fn set_repertoire_activity(
        &mut self,
        dataset: &DatasetId,
        repertoire: &str,
        command_id: &str,
        activity: RepertoireActivity,
    ) -> Result<Value, AppError> {
        if command_id.trim().is_empty() || command_id.len() > 120 {
            return Err(AppError::new(
                "invalid_command_id",
                "A repertoire activity command identity is required.",
            ));
        }
        let command = RepertoireActivityCommand {
            dataset,
            repertoire,
            command_id,
            activity,
        };
        let result = match self.store.repertoire_activity(&command)? {
            RepertoireActivityRead::Settled(result) => result,
            RepertoireActivityRead::Pending { revision, active } => {
                let abandoned = active
                    .as_deref()
                    .filter(|_| activity == RepertoireActivity::Suspended)
                    .map(|attempt| {
                        crate::diagnostics::measure("study.abandon", || {
                            tempo_study::abandon(attempt, attempt.revision)
                        })
                    })
                    .transpose()
                    .map_err(study_input)?;
                self.store
                    .set_repertoire_activity(&command, revision, abandoned)?
            }
        };
        match result {
            RepertoireActivityResult::Applied => {}
            RepertoireActivityResult::StaleDataset => {
                return Err(super::content_preview::stale_dataset());
            }
            RepertoireActivityResult::UnknownRepertoire => return Err(unknown_repertoire()),
            RepertoireActivityResult::CommandConflict => {
                return Err(AppError::new(
                    "command_conflict",
                    "This command was already used for a different repertoire activity change.",
                ));
            }
        }
        Ok(json!({
            "schema_version": CONTENT_WIRE_VERSION, "status": "activity_set",
            "dataset_id": dataset, "repertoire_id": repertoire, "activity": activity,
        }))
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

pub(super) fn suspended_repertoire() -> AppError {
    AppError::new(
        "suspended_repertoire",
        "Resume this repertoire to study it or compare games with it.",
    )
}

pub(super) fn is_suspended(item: &ContentItem, suspended: &BTreeSet<String>) -> bool {
    matches!(&item.definition.kind, ContentKind::Repertoire { repertoire, .. }
        if suspended.contains(repertoire))
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
    snapshot: &ContentState,
    input: &RepertoireInput,
    id: &str,
) -> Result<(String, Catalog, ContentPlan), AppError> {
    check_dataset(snapshot, &input.dataset_id)?;
    let name = validate_name(&input.name)?;
    if let Some(replacement) = &input.repertoire_id {
        let metadata = summaries(&snapshot.catalog, &snapshot.suspended_repertoires)?
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
    let incoming = crate::diagnostics::measure("chess.compile_repertoire", || {
        compile_repertoire(&input.pgn, id, name, input.learner_color, &source)
    })
    .map_err(|e| domain("invalid_pgn", e))?;
    let plan = plan_repertoire_replacement(&snapshot.catalog, &incoming, id)
        .map_err(|e| domain("content_change_error", e))?;
    Ok((source, incoming, plan))
}

fn summaries(
    catalog: &Catalog,
    suspended: &BTreeSet<String>,
) -> Result<BTreeMap<String, Summary>, AppError> {
    use std::collections::btree_map::Entry;

    let mut summaries = BTreeMap::new();
    for item in catalog.items.values() {
        let ContentKind::Repertoire { repertoire, .. } = &item.definition.kind else {
            continue;
        };
        let summary = match summaries.entry(repertoire.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(Summary {
                id: repertoire.clone(),
                name: item.label.clone(),
                learner_color: item
                    .definition
                    .learner_color()
                    .map_err(|error| domain("invalid_content", error))?,
                positions: 0,
                activity: if suspended.contains(repertoire) {
                    RepertoireActivity::Suspended
                } else {
                    RepertoireActivity::Active
                },
            }),
        };
        summary.positions += usize::from(item.active());
    }
    Ok(summaries)
}

fn list(
    dataset: &DatasetId,
    catalog: &Catalog,
    suspended: &BTreeSet<String>,
) -> Result<Value, AppError> {
    Ok(json!({
        "schema_version": CONTENT_WIRE_VERSION, "status": "repertoires", "dataset_id": dataset,
        "repertoires": summaries(catalog, suspended)?.into_values().collect::<Vec<_>>(),
    }))
}

fn preview(
    snapshot: &ContentState,
    incoming: &Catalog,
    plan: &ContentPlan,
    token: String,
) -> Value {
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
