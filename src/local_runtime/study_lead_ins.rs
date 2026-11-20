//! Recover presentation metadata from retained sources without importing content.
use super::{LocalResult, import::Batch};
use crate::application::tactic_catalog_facts::saved_pgn_source;
use serde_json::json;
use std::{collections::BTreeMap, path::Path};
use tempo_chess::{Catalog, ContentItem, ContentKind, LeadInEvidence, archived_tactic_lead_in};
use tempo_store::{SqliteStore, Store};

/// Preview verified context coverage, or apply the same metadata-only operation.
pub fn backfill_study_lead_ins(
    database: &Path,
    transfer: Option<&Path>,
    apply: bool,
) -> LocalResult<()> {
    let legacy = transfer
        .map(Batch::open)
        .transpose()?
        .map(|batch| batch.catalog);
    let store = SqliteStore::open_read_only(database)?;
    let snapshot = store.content_state()?;
    let mut books: BTreeMap<String, Catalog> = BTreeMap::new();
    let mut archives: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut context = BTreeMap::new();
    let mut repertoire = 0;
    let mut tactics = 0;
    let mut conflicts = 0;

    for item in snapshot
        .catalog
        .items
        .values()
        .filter(|item| item.active() && item.opponent_lead_in != LeadInEvidence::Conflicting)
    {
        let source_evidence = match &item.definition.kind {
            ContentKind::Repertoire {
                repertoire: book, ..
            } => {
                if !books.contains_key(book) {
                    let document = store.repertoire_document(book)?;
                    if document.dataset != snapshot.dataset
                        || document.revision != snapshot.content_revision
                    {
                        return Err("The repertoire changed while reading the saved source. Retry the backfill.".into());
                    }
                    let catalog = match document.pgn {
                        Some(pgn) => tempo_chess::compile_repertoire(
                            &pgn,
                            book,
                            "Saved repertoire",
                            item.definition.learner_color()?,
                            "",
                        )
                        .unwrap_or_default(),
                        None => Catalog::default(),
                    };
                    books.insert(book.clone(), catalog);
                }
                matching_context(&books[book], &item.target.0, item)
                    .cloned()
                    .unwrap_or_default()
            }
            ContentKind::Tactic { .. } => {
                let mut evidence = LeadInEvidence::Unavailable;
                for source in &item.provenance {
                    if let Some(lead_in) = legacy
                        .as_ref()
                        .and_then(|catalog| matching_context(catalog, source, item))
                    {
                        evidence.reconcile(lead_in);
                    }
                    if saved_pgn_source(source) {
                        if !archives.contains_key(source) {
                            archives.insert(source.clone(), store.archived_content_pgn(source)?);
                        }
                        if let Some(pgn) = &archives[source] {
                            evidence.reconcile(
                                &archived_tactic_lead_in(pgn, &item.definition).unwrap_or_default(),
                            );
                        }
                    }
                }
                evidence
            }
        };
        let mut lead_in = item.opponent_lead_in.clone();
        lead_in.reconcile(&source_evidence);
        if lead_in == item.opponent_lead_in {
            continue;
        }
        match lead_in {
            LeadInEvidence::Unavailable => continue,
            LeadInEvidence::Verified(_) if item.definition.is_repertoire() => repertoire += 1,
            LeadInEvidence::Verified(_) => tactics += 1,
            LeadInEvidence::Conflicting => conflicts += 1,
        }
        context.insert(item.target.clone(), lead_in);
    }

    if apply && !context.is_empty() {
        SqliteStore::open(database)?.backfill_opponent_lead_ins(
            &snapshot.dataset,
            snapshot.revision,
            &context,
        )?;
    }
    println!(
        "{}",
        json!({
            "status": if apply { "applied" } else { "dry_run" },
            "repertoire": repertoire,
            "tactics": tactics,
            "conflicts": conflicts,
            "already_available": snapshot.catalog.items.values().filter(|item| item.active() && item.opponent_lead_in.verified_move().is_some()).count(),
            "already_conflicting": snapshot.catalog.items.values().filter(|item| item.active() && item.opponent_lead_in == LeadInEvidence::Conflicting).count(),
        })
    );
    Ok(())
}

fn matching_context<'a>(
    catalog: &'a Catalog,
    source: &str,
    item: &ContentItem,
) -> Option<&'a LeadInEvidence> {
    let candidate = catalog.items.get(&tempo_types::TargetId::from(source))?;
    (candidate.definition == item.definition).then_some(&candidate.opponent_lead_in)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backfill_reconciles_a_verified_move_with_newly_available_source_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("study.sqlite");
        let transfer = directory.path().join("transfer.json");
        super::super::init(&database).unwrap();
        let write_transfer = |before, uci| {
            std::fs::write(
                &transfer,
                json!({"schema_version":1,"puzzles":[{
                    "namespace":"fixture", "source_id":"same",
                    "solver_fen":"4k3/8/5n2/8/8/8/4P3/4K3 w - - 0 1",
                    "solution":["e2e4"], "label":"Fixture", "provenance":[],
                    "opponent_lead_ins":[{"before_fen":before,"uci":uci}],
                }]})
                .to_string(),
            )
            .unwrap();
        };
        write_transfer("4k1n1/8/8/8/8/8/4P3/4K3 b - - 0 1", "g8f6");
        super::super::import(&database, &transfer).unwrap();
        let mut app = super::super::open(&database).unwrap();
        let attempt = app.next_exercise().unwrap().unwrap().attempt;
        let before = app.snapshot().unwrap();
        assert!(matches!(
            before.catalog.items[&attempt.target].opponent_lead_in,
            LeadInEvidence::Verified(_)
        ));
        drop(app);

        write_transfer("4k3/8/8/7n/8/8/4P3/4K3 b - - 0 1", "h5f6");
        let read = SqliteStore::open_read_only(&database).unwrap();
        backfill_study_lead_ins(&database, Some(&transfer), false).unwrap();
        assert_eq!(read.snapshot().unwrap(), before);
        backfill_study_lead_ins(&database, Some(&transfer), true).unwrap();
        let after = read.snapshot().unwrap();
        assert_eq!(
            after.catalog.items[&attempt.target].opponent_lead_in,
            LeadInEvidence::Conflicting
        );
        assert_eq!(after.learning, before.learning);
        assert_eq!(after.content_history, before.content_history);
        assert_eq!(after.attempts, before.attempts);
        assert_eq!(after.active_attempt, before.active_attempt);
        backfill_study_lead_ins(&database, Some(&transfer), true).unwrap();
        assert_eq!(read.snapshot().unwrap(), after);
    }
}
