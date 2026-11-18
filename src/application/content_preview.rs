//! Bind exact preview input to content revision and publish a typed catalog plan.
use super::Tempo;
use crate::{AppError, app_error::study_input};
use serde::Serialize;
use tempo_chess::Catalog;
use tempo_store::{Commit, ContentReceipt, ContentSource, Snapshot, StoredAttempt, Write};
use tempo_types::{DatasetId, Revision};

impl Tempo {
    pub(super) fn commit_catalog(
        &mut self,
        snapshot: &Snapshot,
        catalog: &Catalog,
        mut writes: Vec<Write>,
        source: Option<ContentSource<'_>>,
        receipt: Option<ContentReceipt<'_>>,
    ) -> Result<(), AppError> {
        let catalog_changed = catalog != &snapshot.catalog;
        if catalog_changed && let Some(active) = &snapshot.active_attempt {
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
        if catalog_changed {
            writes.insert(0, Write::Catalog(catalog.clone()));
        } else if receipt.is_none() {
            return Ok(());
        }
        self.store.commit_content(
            Commit {
                expected_revision: snapshot.revision,
                expected_dataset: snapshot.dataset.clone(),
                writes,
            },
            source.filter(|source| {
                catalog_changed || matches!(source, ContentSource::RepertoireDocument { .. })
            }),
            receipt,
        )?;
        Ok(())
    }
}

pub(super) fn check_dataset(snapshot: &Snapshot, dataset: &DatasetId) -> Result<(), AppError> {
    if &snapshot.dataset != dataset {
        return Err(stale_dataset());
    }
    Ok(())
}

pub(super) fn stale_dataset() -> AppError {
    AppError::new(
        "stale_dataset",
        "This command belongs to an earlier dataset. Reload Tempo.",
    )
}

pub(super) fn stale_preview() -> AppError {
    AppError::new("stale_preview", "Preview this PGN again before saving.")
}

pub(super) fn signature(
    key: &[u8; 32],
    revision: Revision,
    id: &str,
    input: &impl Serialize,
) -> String {
    let bytes = serde_json::to_vec(&(revision, id, input)).expect("serializable content preview");
    blake3::keyed_hash(key, &bytes).to_hex().to_string()
}

pub(super) fn preview_id(
    key: &[u8; 32],
    revision: Revision,
    id: &str,
    input: &impl Serialize,
) -> String {
    let mac = signature(key, revision, id, input);
    format!("{}:{id}:{mac}", revision.0)
}

pub(super) fn read_preview<'a>(
    key: &[u8; 32],
    token: &'a str,
    input: &impl Serialize,
) -> Result<(Revision, &'a str), AppError> {
    let mut fields = token.splitn(3, ':');
    let revision = fields
        .next()
        .and_then(|r| r.parse().ok())
        .map(Revision)
        .ok_or_else(stale_preview)?;
    let id = fields.next().unwrap_or_default();
    let mac = fields.next().unwrap_or_default();
    if signature(key, revision, id, input) != mac {
        return Err(stale_preview());
    }
    Ok((revision, id))
}

pub(super) fn source_id(pgn: &str) -> String {
    format!("pgn:{}", blake3::hash(pgn.as_bytes()).to_hex())
}
