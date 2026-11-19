//! Decode the transfer format and report dispositions for one atomic import.
use super::LocalResult;
use crate::{Tempo, application::tactic_catalog_facts::pack_memberships};
use serde::{Deserialize, Serialize};
use std::{fs::File, io, path::Path};
use tempo_chess::{
    Catalog, ChangeKind, ContentChange, ContentStatus, TacticRecord, compile_tactics,
};
use tempo_types::{ContentVersion, TargetId};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pack {
    schema_version: u32,
    puzzles: Vec<SourcePuzzle>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePuzzle {
    namespace: String,
    source_id: String,
    solver_fen: String,
    solution: Vec<String>,
    label: String,
    provenance: Vec<String>,
}

pub(super) struct Batch {
    catalog: Catalog,
    identities: Vec<SourceIdentity>,
}

impl Batch {
    pub(super) fn open(path: &Path) -> LocalResult<Self> {
        let pack: Pack = serde_json::from_reader(io::BufReader::new(File::open(path)?))?;
        if pack.schema_version != 1 {
            return Err("unsupported tactic transfer schema version".into());
        }
        let records = pack
            .puzzles
            .into_iter()
            .map(|row| TacticRecord {
                namespace: row.namespace,
                source_id: row.source_id,
                solver_fen: row.solver_fen,
                solution: row.solution,
                label: row.label,
                provenance: row.provenance,
            })
            .collect::<Vec<_>>();
        let catalog = compile_tactics(&records)?;
        let mut identities = records
            .into_iter()
            .map(|row| SourceIdentity {
                source_token: row.target_id(),
                namespace: row.namespace,
                source_id: row.source_id,
            })
            .collect::<Vec<_>>();
        identities.sort_by(|a, b| a.source_token.cmp(&b.source_token));
        Ok(Self {
            catalog,
            identities,
        })
    }
}

#[derive(Serialize)]
struct SourceIdentity {
    namespace: String,
    source_id: String,
    source_token: TargetId,
}

#[derive(Serialize)]
struct Mapping {
    #[serde(flatten)]
    source: SourceIdentity,
    target: TargetId,
    version: ContentVersion,
    status: ContentStatus,
    alias: bool,
}

#[derive(Serialize)]
struct RetainedMembershipGap {
    target: TargetId,
    source_id: String,
    theme: String,
    difficulty: &'static str,
    ordinal: usize,
    position: usize,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    dataset: String,
    changes: Vec<ContentChange>,
    sources: Vec<Mapping>,
    historical_learning_mapped: u32,
    final_tactic_count: usize,
    already_present_tactic_count: usize,
    retained_membership_gaps: Vec<RetainedMembershipGap>,
}

pub(super) fn run(app: &mut Tempo, batch: Batch) -> LocalResult<()> {
    let dataset = app.snapshot()?.dataset;
    let plan = app.apply_tactic_transfer(&batch.catalog)?;
    let final_tactic_count = plan
        .catalog
        .items
        .values()
        .filter(|item| !item.definition.is_repertoire())
        .count();
    let added = plan
        .changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Added)
        .count();
    let mut retained_membership_gaps = Vec::new();
    let sources = batch
        .identities
        .into_iter()
        .zip(&plan.changes)
        .map(|(source, change)| {
            let item = &plan.catalog.items[&change.target];
            if change.kind == ChangeKind::Unchanged
                && let Some(incoming) = batch.catalog.items.get(&source.source_token)
            {
                let retained: Vec<_> = pack_memberships(&item.provenance).collect();
                for fact in pack_memberships(&incoming.provenance) {
                    if retained.contains(&fact) {
                        continue;
                    }
                    let difficulty = match fact.difficulty {
                        tempo_queue::PackDifficulty::Easy => "easy",
                        tempo_queue::PackDifficulty::Medium => "medium",
                        tempo_queue::PackDifficulty::Hard => "hard",
                    };
                    retained_membership_gaps.push(RetainedMembershipGap {
                        target: item.target.clone(),
                        source_id: source.source_id.clone(),
                        theme: fact.theme.to_owned(),
                        difficulty,
                        ordinal: fact.ordinal,
                        position: fact.position,
                    });
                }
            }
            Mapping {
                alias: source.source_token != item.target,
                source,
                target: item.target.clone(),
                version: item.version,
                status: item.status,
            }
        })
        .collect();
    let report = Report {
        schema_version: 1,
        dataset: dataset.0,
        changes: plan.changes,
        sources,
        historical_learning_mapped: 0,
        final_tactic_count,
        already_present_tactic_count: final_tactic_count - added,
        retained_membership_gaps,
    };
    use std::io::Write;
    let mut output = io::stdout().lock();
    serde_json::to_writer_pretty(&mut output, &report)?;
    writeln!(output)?;
    Ok(())
}
