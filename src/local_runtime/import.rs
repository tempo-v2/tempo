//! Decode the transfer format and report dispositions for one atomic import.
use super::LocalResult;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs::File, io, path::Path};
use tempo::Tempo;
use tempo_chess::{Catalog, ContentChange, ContentStatus, TacticRecord, compile_tactics};
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
        let identities = records
            .into_iter()
            .map(|row| SourceIdentity {
                source_token: row.target_id(),
                namespace: row.namespace,
                source_id: row.source_id,
            })
            .collect();
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
struct Report {
    schema_version: u32,
    dataset: String,
    changes: Vec<ContentChange>,
    sources: Vec<Mapping>,
    historical_learning_mapped: u32,
}

pub(super) fn run(app: &mut Tempo, batch: Batch) -> LocalResult<()> {
    let dataset = app.snapshot()?.dataset;
    let plan = app.apply_catalog(&dataset, &batch.catalog)?;
    let by_source = plan
        .catalog
        .items
        .values()
        .flat_map(|item| {
            item.provenance
                .iter()
                .map(move |source| (source.as_str(), item))
        })
        .collect::<BTreeMap<_, _>>();
    let sources = batch
        .identities
        .into_iter()
        .map(|source| {
            let item = by_source
                .get(source.source_token.0.as_str())
                .ok_or("imported source is missing a retained target")?;
            Ok(Mapping {
                alias: source.source_token != item.target,
                source,
                target: item.target.clone(),
                version: item.version,
                status: item.status,
            })
        })
        .collect::<Result<_, &str>>()?;
    let report = Report {
        schema_version: 1,
        dataset: dataset.0,
        changes: plan.changes,
        sources,
        historical_learning_mapped: 0,
    };
    use std::io::Write;
    let mut output = io::stdout().lock();
    serde_json::to_writer_pretty(&mut output, &report)?;
    writeln!(output)?;
    Ok(())
}
