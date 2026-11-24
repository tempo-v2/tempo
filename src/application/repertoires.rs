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
    game_model::{AuthoringSurface, model_input_stamp},
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
    ContentReceipt, ContentSource, ContentState, CoverageEditBasis, PriorRepertoireReset,
    RepertoireActivity, RepertoireActivityCommand, RepertoireActivityRead,
    RepertoireActivityResult, Write, model_targets,
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
        self.handle_repertoire_on(command, AuthoringSurface::Desktop)
    }

    pub(crate) fn handle_repertoire_on(
        &mut self,
        command: RepertoireCommand,
        surface: AuthoringSurface,
    ) -> Value {
        let _diagnostic = crate::diagnostics::Span::new("app.repertoires.handle_repertoire");
        self.repertoire(command, surface)
            .unwrap_or_else(|error| json!(ErrorView::for_content(error)))
    }

    /// Authorize an append-only gap draft against the same content snapshot
    /// that the signed Save will commit.
    pub(super) fn gap_response_token(
        &self,
        input: &RepertoireInput,
        id: &str,
        base_revision: Revision,
        surface: AuthoringSurface,
        forecast: Option<&CoverageEditBasis>,
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
        Ok(match (surface, forecast) {
            (surface, Some(basis)) => forecast_preview(&key, revision, id, input, surface, basis),
            (AuthoringSurface::Desktop, None) => document_preview(&key, revision, id, input),
            (AuthoringSurface::PhoneBuild, None) => return Err(stale_preview()),
        })
    }

    fn repertoire(
        &mut self,
        command: RepertoireCommand,
        surface: AuthoringSurface,
    ) -> Result<Value, AppError> {
        if let RepertoireCommand::Save { input, token } = &command
            && is_phone_forecast(token)
        {
            return self.replay_phone_forecast(input, token);
        }
        if surface == AuthoringSurface::PhoneBuild {
            return Err(phone_build_retired());
        }
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
            return self.replay_repertoire_reset(dataset_id, repertoire_id, command_id);
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
                let forecast = read_forecast_preview(&key, &token, &input)?;
                let (revision_in_token, id) = if let Some((revision, id, _)) = &forecast {
                    (*revision, *id)
                } else if let Some(versioned) = token.strip_prefix(DOCUMENT_PREVIEW) {
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
                        forecast: forecast.as_ref().map(|(_, _, basis)| basis),
                    }),
                    Some(receipt),
                )?;
                Ok(reply)
            }
        }
    }

    fn replay_phone_forecast(
        &self,
        input: &RepertoireInput,
        token: &str,
    ) -> Result<Value, AppError> {
        let key = self
            .daily
            .as_ref()
            .ok_or_else(|| AppError::new("unsupported", "Use the local study release."))?
            .preview_key;
        if let Some(signed) = token.strip_prefix(PHONE_FORECAST) {
            read_preview(&key, signed, &(PHONE_FORECAST, input))?;
        } else {
            read_forecast_preview(&key, token, input)?.ok_or_else(stale_preview)?;
        }
        if self.store.dataset_id()? != input.dataset_id {
            return Err(super::content_preview::stale_dataset());
        }
        let saved = self
            .store
            .content_receipt(token)?
            .ok_or_else(phone_build_retired)?;
        serde_json::from_str(&saved).map_err(|error| domain("invalid_receipt", error))
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

    fn replay_repertoire_reset(
        &self,
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
        match self
            .store
            .accepted_repertoire_reset(dataset, repertoire, command_id)?
        {
            PriorRepertoireReset::Replayed => {}
            PriorRepertoireReset::NotAccepted => {
                return Err(AppError::new(
                    "retired_feature",
                    "Resetting repertoire progress has been retired.",
                ));
            }
            PriorRepertoireReset::StaleDataset => {
                return Err(super::content_preview::stale_dataset());
            }
            PriorRepertoireReset::CommandConflict => {
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
const PHONE_FORECAST: &str = "phone-forecast-v1:";

const DESKTOP_FORECAST_V2: &str = "desktop-forecast-v2:";
const PHONE_FORECAST_V2: &str = "phone-forecast-v2:";

fn phone_build_retired() -> AppError {
    AppError::new("retired_feature", "Phone Build has been retired.")
}

fn is_phone_forecast(token: &str) -> bool {
    token.starts_with(PHONE_FORECAST) || token.starts_with(PHONE_FORECAST_V2)
}

fn forecast_preview(
    key: &[u8; 32],
    revision: Revision,
    id: &str,
    input: &RepertoireInput,
    surface: AuthoringSurface,
    basis: &CoverageEditBasis,
) -> String {
    let purpose = match surface {
        AuthoringSurface::Desktop => DESKTOP_FORECAST_V2,
        AuthoringSurface::PhoneBuild => PHONE_FORECAST_V2,
    };
    let signed = preview_id(key, revision, id, &(purpose, input, basis));
    format!(
        "{purpose}{}:{}:{signed}",
        basis.scope_revision, basis.report_id
    )
}

fn read_forecast_preview<'a>(
    key: &[u8; 32],
    token: &'a str,
    input: &RepertoireInput,
) -> Result<Option<(Revision, &'a str, CoverageEditBasis)>, AppError> {
    let purpose = if token.starts_with(PHONE_FORECAST_V2) {
        PHONE_FORECAST_V2
    } else if token.starts_with(DESKTOP_FORECAST_V2) {
        DESKTOP_FORECAST_V2
    } else {
        return Ok(None);
    };
    let mut fields = token[purpose.len()..].splitn(3, ':');
    let scope_revision = fields
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(stale_preview)?;
    let report_id = fields.next().ok_or_else(stale_preview)?.to_owned();
    let signed = fields.next().ok_or_else(stale_preview)?;
    let basis = CoverageEditBasis {
        report_id,
        scope_revision,
    };
    let (revision, id) = read_preview(key, signed, &(purpose, input, &basis))?;
    Ok(Some((revision, id, basis)))
}

fn document_preview(
    key: &[u8; 32],
    revision: tempo_types::Revision,
    id: &str,
    input: &RepertoireInput,
) -> String {
    scoped_preview(key, revision, id, input, DOCUMENT_PREVIEW)
}

fn scoped_preview(
    key: &[u8; 32],
    revision: Revision,
    id: &str,
    input: &RepertoireInput,
    purpose: &str,
) -> String {
    let signed = preview_id(key, revision, id, &(purpose, input));
    format!("{purpose}{signed}")
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

#[cfg(test)]
mod phone_recovery_tests {
    use super::*;
    use crate::{DailyComponents, IntroductionLimits};
    use tempo_learning::LearningPolicy;
    use tempo_store::{Commit, SqliteStore, Store};

    const KEY: [u8; 32] = [42; 32];

    #[test]
    fn phone_save_replays_exact_signed_receipts_and_cannot_admit_new_work() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("phone-receipts.sqlite");
        let dataset = DatasetId::from("phone-recovery");
        let policy = DailyLearning::default();
        let mut store = SqliteStore::create(
            &path,
            dataset.clone(),
            Catalog::default(),
            policy.settings(),
        )
        .unwrap();
        let input = RepertoireInput {
            dataset_id: dataset.clone(),
            repertoire_id: Some("earlier-book".into()),
            name: "Earlier book".into(),
            learner_color: LearnerColor::White,
            pgn: "Historical content is not parsed during recovery.".into(),
        };
        let basis = CoverageEditBasis {
            report_id: "removed-report".into(),
            scope_revision: 25,
        };
        let modern = |input: &RepertoireInput| {
            forecast_preview(
                &KEY,
                Revision(42),
                "earlier-book",
                input,
                AuthoringSurface::PhoneBuild,
                &basis,
            )
        };
        let tokens = [
            scoped_preview(&KEY, Revision(41), "earlier-book", &input, PHONE_FORECAST),
            modern(&input),
        ];
        let reply = json!({
            "schema_version": CONTENT_WIRE_VERSION, "status": "repertoires",
            "dataset_id": dataset, "repertoires": [],
        });
        for token in &tokens {
            let before = store.snapshot().unwrap();
            store
                .commit_content(
                    Commit {
                        expected_dataset: dataset.clone(),
                        expected_revision: before.revision,
                        writes: vec![],
                    },
                    None,
                    Some(ContentReceipt {
                        request: token,
                        reply: &reply.to_string(),
                    }),
                )
                .unwrap();
        }
        let before = store.snapshot().unwrap();
        drop(store);
        let mut app = Tempo::daily(
            Box::new(SqliteStore::open(&path).unwrap()),
            DailyComponents {
                learning: Box::new(policy),
                ..crate::components::test_components()
            },
            IntroductionLimits::default(),
            KEY,
            0,
        )
        .unwrap();
        let save = |input, token| RepertoireCommand::Save { input, token };
        for token in tokens {
            assert_eq!(
                app.handle_repertoire_on(
                    save(input.clone(), token.clone()),
                    AuthoringSurface::PhoneBuild
                ),
                reply
            );
            let changed = RepertoireInput {
                name: "Changed after signing".into(),
                ..input.clone()
            };
            assert_eq!(
                app.handle_repertoire_on(save(changed, token), AuthoringSurface::PhoneBuild)["code"],
                "stale_preview"
            );
        }
        let foreign = RepertoireInput {
            dataset_id: "other".into(),
            ..input.clone()
        };
        let token = modern(&foreign);
        assert_eq!(
            app.handle_repertoire_on(save(foreign, token), AuthoringSurface::PhoneBuild)["code"],
            "stale_dataset"
        );

        let fresh = RepertoireInput {
            pgn: "1. e4 e5 2. Nf3 *".into(),
            repertoire_id: None,
            ..input
        };
        let token = modern(&fresh);
        for surface in [AuthoringSurface::PhoneBuild, AuthoringSurface::Desktop] {
            assert_eq!(
                app.handle_repertoire_on(save(fresh.clone(), token.clone()), surface)["code"],
                "retired_feature"
            );
        }
        assert_eq!(
            app.handle_repertoire_on(RepertoireCommand::List, AuthoringSurface::PhoneBuild)["code"],
            "retired_feature"
        );
        let preview = app.handle_repertoire(RepertoireCommand::Preview(fresh.clone()));
        let token = preview["token"].as_str().unwrap().to_owned();
        assert_eq!(
            app.handle_repertoire_on(
                save(fresh.clone(), token.clone()),
                AuthoringSurface::PhoneBuild
            )["code"],
            "retired_feature"
        );
        assert_eq!(app.snapshot().unwrap(), before);
        assert_eq!(
            app.handle_repertoire(save(fresh, token))["status"],
            "repertoires"
        );
        assert_ne!(app.snapshot().unwrap(), before);
    }
}
