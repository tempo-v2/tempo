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
    #[serde(default)]
    opponent_lead_ins: Vec<tempo_chess::OpponentLeadIn>,
}

pub(super) struct Batch {
    pub(super) catalog: Catalog,
    identities: Vec<SourceIdentity>,
}

impl Batch {
    pub(super) fn open(path: &Path) -> LocalResult<Self> {
        let pack: Pack = serde_json::from_reader(io::BufReader::new(File::open(path)?))?;
        if pack.schema_version != 1 {
            return Err("unsupported tactic transfer schema version".into());
        }
        let (records, context): (Vec<_>, Vec<_>) = pack
            .puzzles
            .into_iter()
            .map(|row| {
                (
                    TacticRecord {
                        namespace: row.namespace,
                        source_id: row.source_id,
                        solver_fen: row.solver_fen,
                        solution: row.solution,
                        label: row.label,
                        provenance: row.provenance,
                    },
                    row.opponent_lead_ins,
                )
            })
            .unzip();
        let mut catalog = compile_tactics(&records)?;
        for (record, lead_ins) in records.iter().zip(context) {
            let item = catalog
                .items
                .get_mut(&record.target_id())
                .expect("compiled target");
            for lead_in in lead_ins {
                let verified = tempo_chess::OpponentLeadIn::verified(
                    &lead_in.before_fen,
                    &lead_in.uci,
                    &record.solver_fen,
                );
                item.opponent_lead_in.reconcile(&verified.into());
            }
        }
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

fn apply(app: &mut Tempo, batch: Batch) -> LocalResult<Report> {
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
            if matches!(change.kind, ChangeKind::Unchanged | ChangeKind::Metadata)
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
    Ok(Report {
        schema_version: 1,
        dataset: dataset.0,
        changes: plan.changes,
        sources,
        historical_learning_mapped: 0,
        final_tactic_count,
        already_present_tactic_count: final_tactic_count - added,
        retained_membership_gaps,
    })
}

pub(super) fn run(app: &mut Tempo, batch: Batch) -> LocalResult<()> {
    let report = apply(app, batch)?;
    use std::io::Write;
    let mut output = io::stdout().lock();
    serde_json::to_writer_pretty(&mut output, &report)?;
    writeln!(output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn batch(directory: &Path, label: &str, membership: Option<(&str, usize)>) -> Batch {
        let metadata = membership.map(|(theme, position)| {
            format!(
                "tempo1:catalog-metadata:{}",
                json!({"Motif":theme,"Difficulty":"easy",
                "DeckId":format!("{theme}-easy-01"),"DeckPosition":position})
            )
        });
        let mut provenance = vec!["https://example.invalid/source".to_owned()];
        provenance.extend(metadata);
        let transfer = directory.join("transfer.json");
        std::fs::write(
            &transfer,
            json!({"schema_version":1,"puzzles":[{
                "namespace":"catalog-test","source_id":"puzzle",
                "solver_fen":"4k3/8/8/8/8/8/4P3/4K3 w - - 0 1","solution":["e2e4"],
                "label":label,"provenance":provenance,
            }]})
            .to_string(),
        )
        .unwrap();
        Batch::open(&transfer).unwrap()
    }

    fn daily(directory: &Path, name: &str) -> Tempo {
        let database = directory.join(name);
        super::super::init(&database).unwrap();
        let mut app = super::super::open(&database).unwrap();
        app.set_local_tactic_preferences(
            10,
            Some(crate::TacticThemes::Selected {
                theme_ids: ["fork".to_owned()].into(),
            }),
        );
        app
    }

    #[test]
    fn transfer_membership_controls_actual_daily_admission_and_retained_data_is_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        let mut accepted = daily(path, "accepted.sqlite");
        let report = apply(&mut accepted, batch(path, "First label", Some(("fork", 2)))).unwrap();
        let target = &report.sources[0].target;
        let proposal = accepted.next_exercise().unwrap().unwrap();
        assert_eq!(&proposal.attempt.target, target);

        let mut equivalent = daily(path, "equivalent.sqlite");
        apply(
            &mut equivalent,
            batch(path, "Renamed label", Some(("fork", 2))),
        )
        .unwrap();
        assert_eq!(
            &equivalent.next_exercise().unwrap().unwrap().attempt.target,
            target
        );

        let mut other = daily(path, "other.sqlite");
        apply(&mut other, batch(path, "First label", Some(("pin", 2)))).unwrap();
        assert!(other.next_exercise().unwrap().is_none());

        let mut retained = daily(path, "retained.sqlite");
        apply(&mut retained, batch(path, "Retained label", None)).unwrap();
        let before = retained.snapshot().unwrap();
        for _ in 0..2 {
            let imported =
                apply(&mut retained, batch(path, "New label", Some(("fork", 2)))).unwrap();
            assert_eq!(imported.retained_membership_gaps.len(), 1);
            assert_eq!(&imported.retained_membership_gaps[0].target, target);
            assert_eq!(imported.retained_membership_gaps[0].position, 2);
            assert_eq!(retained.snapshot().unwrap(), before);
            assert!(retained.next_exercise().unwrap().is_none());
        }
    }

    #[test]
    fn matching_transfers_reconcile_setup_without_replacing_issued_facts() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("transfer.sqlite");
        super::super::init(&database).unwrap();
        let mut app = super::super::open(&database).unwrap();
        let transfer = directory.path().join("transfer.json");
        let load = |id, fen, uci| {
            std::fs::write(
                &transfer,
                json!({"schema_version":1,"puzzles":[{
                    "namespace":"catalog-test","source_id":id,
                    "solver_fen":"4k3/8/5n2/8/8/8/4P3/4K3 w - - 0 1","solution":["e2e4"],
                    "label":"Example","provenance":[],
                    "opponent_lead_ins":[{"before_fen":fen,"uci":uci}],
                }]})
                .to_string(),
            )
            .unwrap();
            Batch::open(&transfer).unwrap()
        };
        let first = apply(
            &mut app,
            load("a", "4k1n1/8/8/8/8/8/4P3/4K3 b - - 0 1", "g8f6"),
        )
        .unwrap();
        let target = &first.sources[0].target;
        let issued = app.next_exercise().unwrap().unwrap().attempt;
        let before = app.snapshot().unwrap();
        assert_eq!(
            before.attempts[&issued.id]
                .opponent_lead_in
                .as_ref()
                .unwrap()
                .uci,
            "g8f6"
        );

        let revision = apply(
            &mut app,
            load("b", "4k3/8/8/7n/8/8/4P3/4K3 b - - 0 1", "h5f6"),
        )
        .unwrap();
        assert_eq!(revision.changes[0].kind, ChangeKind::Metadata);
        let after = app.snapshot().unwrap();
        assert_eq!(
            after.catalog.items[target].opponent_lead_in,
            tempo_chess::LeadInEvidence::Conflicting
        );
        assert_eq!(
            after.catalog.items[target].provenance,
            before.catalog.items[target].provenance
        );
        assert_eq!(after.content_history, before.content_history);
        assert_eq!(after.learning, before.learning);
        assert_eq!(after.active_attempt, before.active_attempt);
        assert_eq!(after.attempts, before.attempts);
    }
}
