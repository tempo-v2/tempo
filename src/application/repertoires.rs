//! Preview and replace one complete repertoire, fenced independently of study reviews.
use super::Tempo;
use crate::{
    AppError,
    app_error::{domain, study_input},
    commands::{RepertoireCommand, RepertoireInput},
    views::StudyView,
    wire::STUDY_WIRE_VERSION,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempo_chess::{
    Catalog, ChangeKind, ContentKind, ContentPlan, LearnerColor, compile_repertoire,
    plan_repertoire_replacement,
};
use tempo_learning::{DailyLearning, LearningState};
use tempo_store::{Commit, ContentReceipt, ContentSource, Snapshot, StoredAttempt, Write};
use tempo_types::{DatasetId, Revision};

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
            .unwrap_or_else(|error| json!(StudyView::from(error)))
    }

    fn repertoire(&mut self, command: RepertoireCommand) -> Result<Value, AppError> {
        let key = self
            .daily
            .as_ref()
            .ok_or_else(|| AppError::new("unsupported", "Use the local study release."))?
            .preview_key;
        let (snapshot, revision) = self.store.content_state()?;
        match command {
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
                    return Err(AppError::new(
                        "unknown_repertoire",
                        "Choose an existing repertoire.",
                    ));
                }
                if catalog != snapshot.catalog {
                    self.commit_repertoire(&snapshot, &catalog, vec![], None, None)?;
                }
                Ok(list(&snapshot.dataset, &catalog))
            }
            RepertoireCommand::Preview(input) => {
                let id = input.repertoire_id.clone().unwrap_or_else(|| {
                    let digest = signature(&key, revision, "", &input);
                    format!("repertoire-{}", &digest[..32])
                });
                let mac = signature(&key, revision, &id, &input);
                let token = format!("{}:{id}:{mac}", revision.0);
                let (_, incoming, plan) = prepare(&snapshot, &input, &id)?;
                Ok(preview(&snapshot, &incoming, &plan, token))
            }
            RepertoireCommand::Save { input, token } => {
                let mut fields = token.splitn(3, ':');
                let revision_in_token = fields.next().and_then(|r| r.parse().ok()).map(Revision);
                let id = fields.next().unwrap_or_default();
                let mac = fields.next().unwrap_or_default();
                let revision_in_token = revision_in_token
                    .filter(|r| signature(&key, *r, id, &input) == mac)
                    .ok_or_else(stale_preview)?;
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
                if plan.catalog != snapshot.catalog {
                    let learning = plan
                        .changes
                        .iter()
                        .filter_map(|change| {
                            let state = match change.kind {
                                ChangeKind::Added => LearningState::default(),
                                ChangeKind::Semantic => DailyLearning::changed_answer(
                                    &snapshot.learning[&change.target],
                                ),
                                _ => return None,
                            };
                            Some(Write::Learning {
                                target: change.target.clone(),
                                state,
                            })
                        })
                        .collect();
                    self.commit_repertoire(
                        &snapshot,
                        &plan.catalog,
                        learning,
                        Some(ContentSource {
                            id: &source,
                            pgn: &input.pgn,
                        }),
                        Some(receipt),
                    )?;
                } else {
                    self.store.commit_content(
                        Commit {
                            expected_revision: snapshot.revision,
                            expected_dataset: snapshot.dataset.clone(),
                            writes: vec![],
                        },
                        None,
                        Some(receipt),
                    )?;
                }
                Ok(reply)
            }
        }
    }

    fn commit_repertoire(
        &mut self,
        snapshot: &Snapshot,
        catalog: &Catalog,
        mut writes: Vec<Write>,
        source: Option<ContentSource<'_>>,
        receipt: Option<ContentReceipt<'_>>,
    ) -> Result<(), AppError> {
        if let Some(active) = &snapshot.active_attempt {
            let record = &snapshot.attempts[active];
            let old = &record.attempt;
            if catalog
                .items
                .get(&old.target)
                .is_none_or(|item| !item.active() || item.version != old.content_version)
            {
                let attempt = tempo_study::abandon(old, old.revision).map_err(study_input)?;
                writes.push(Write::Progress(StoredAttempt {
                    attempt,
                    ..record.clone()
                }));
                writes.push(Write::Active(None));
            }
        }
        writes.insert(0, Write::Catalog(catalog.clone()));
        self.store.commit_content(
            Commit {
                expected_revision: snapshot.revision,
                expected_dataset: snapshot.dataset.clone(),
                writes,
            },
            source,
            receipt,
        )?;
        Ok(())
    }
}

fn check_dataset(snapshot: &Snapshot, dataset: &DatasetId) -> Result<(), AppError> {
    if &snapshot.dataset != dataset {
        return Err(AppError::new(
            "stale_dataset",
            "This command belongs to an earlier dataset. Reload Tempo.",
        ));
    }
    Ok(())
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

fn stale_preview() -> AppError {
    AppError::new("stale_preview", "Preview this PGN again before saving.")
}

fn signature(key: &[u8; 32], revision: Revision, id: &str, input: &RepertoireInput) -> String {
    let bytes =
        serde_json::to_vec(&(revision, id, input)).expect("serializable repertoire preview");
    blake3::keyed_hash(key, &bytes).to_hex().to_string()
}

fn prepare(
    snapshot: &Snapshot,
    input: &RepertoireInput,
    id: &str,
) -> Result<(String, Catalog, ContentPlan), AppError> {
    check_dataset(snapshot, &input.dataset_id)?;
    let name = validate_name(&input.name)?;
    if input.pgn.len() > 256 * 1024 {
        return Err(AppError::new(
            "pgn_too_large",
            "The PGN must be at most 256 KiB.",
        ));
    }
    if let Some(replacement) = &input.repertoire_id {
        let metadata = summaries(&snapshot.catalog)
            .remove(replacement)
            .ok_or_else(|| AppError::new("unknown_repertoire", "Choose an existing repertoire."))?;
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
    let source = format!("pgn:{}", blake3::hash(input.pgn.as_bytes()).to_hex());
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
        let learner_color =
            if item.definition.decisions[0].fen.split_whitespace().nth(1) == Some("b") {
                LearnerColor::Black
            } else {
                LearnerColor::White
            };
        let summary = summaries
            .entry(repertoire.clone())
            .or_insert_with(|| Summary {
                id: repertoire.clone(),
                name: item.label.clone(),
                learner_color,
                positions: 0,
            });
        summary.positions += usize::from(item.active());
    }
    summaries
}

fn list(dataset: &DatasetId, catalog: &Catalog) -> Value {
    json!({
        "schema_version": STUDY_WIRE_VERSION, "status": "repertoires", "dataset_id": dataset,
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
        "schema_version": STUDY_WIRE_VERSION, "status": "preview", "token": token,
        "positions": incoming.items.len(), "routes": incoming.lines.len(),
        "added": added, "removed": removed, "changed": changed, "affected_learning": affected_learning,
    })
}
