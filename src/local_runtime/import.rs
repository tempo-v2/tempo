//! Decode the transfer format and report dispositions for one atomic import.
use super::LocalResult;
use crate::Tempo;
use serde::{Deserialize, Serialize};
use std::{fs::File, io, path::Path};
use tempo_chess::{
    Catalog, ChangeKind, ContentChange, ContentStatus, GameCapture, TacticPolicyFacts,
    TacticRecord, compile_tactics,
};
use tempo_types::{ContentVersion, TargetId};

const LEGACY_PACK: &str = "tempo1:catalog-metadata:";

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LegacyPackMember<'a> {
    motif: &'a str,
    difficulty: &'a str,
    deck_id: &'a str,
    deck_position: usize,
}

// Preserve the schema-1 offline report's per-pack diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PackDifficulty {
    Easy,
    Medium,
    Hard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PackMembership<'a> {
    theme: &'a str,
    difficulty: PackDifficulty,
    ordinal: usize,
    position: usize,
}

fn pack_memberships(provenance: &[String]) -> impl Iterator<Item = PackMembership<'_>> {
    provenance.iter().filter_map(|entry| {
        let member: LegacyPackMember<'_> =
            serde_json::from_str(entry.strip_prefix(LEGACY_PACK)?).ok()?;
        let difficulty = match member.difficulty {
            "easy" => PackDifficulty::Easy,
            "medium" => PackDifficulty::Medium,
            "hard" => PackDifficulty::Hard,
            _ => return None,
        };
        if !tempo_chess::known_tactic_theme(member.motif) {
            return None;
        }
        let (pack, ordinal) = member.deck_id.rsplit_once('-')?;
        let (theme, pack_difficulty) = pack.rsplit_once('-')?;
        let ordinal = ordinal.parse().ok()?;
        (theme == member.motif
            && pack_difficulty == member.difficulty
            && ordinal > 0
            && member.deck_position > 0)
            .then_some(PackMembership {
                theme,
                difficulty,
                ordinal,
                position: member.deck_position,
            })
    })
}

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
    #[serde(default)]
    legacy_game_captures: Vec<LegacyGameCaptureInput>,
}

/// One captured request from a Tempo 1 game record, paired before import.
/// Its definition has to match the normalized card before accepting the fact.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyGameCaptureInput {
    capture_id: String,
    solver_fen: String,
    solution: Vec<String>,
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
                    (row.opponent_lead_ins, row.legacy_game_captures),
                )
            })
            .unzip();
        let mut catalog = compile_tactics(&records)?;
        for (record, (lead_ins, legacy_game_captures)) in records.iter().zip(context) {
            let item = catalog
                .items
                .get_mut(&record.target_id())
                .expect("compiled target");
            item.tactic_policy_facts = Some(TacticPolicyFacts::from_legacy_metadata(
                record.provenance.iter().map(String::as_str),
            ));
            for lead_in in lead_ins {
                let verified = tempo_chess::OpponentLeadIn::verified(
                    &lead_in.before_fen,
                    &lead_in.uci,
                    &record.solver_fen,
                );
                item.opponent_lead_in.reconcile(&verified.into());
            }
            if record.namespace == "tempo1-card" {
                item.game_capture = legacy_game_captures.into_iter().find_map(|capture| {
                    if capture.capture_id.trim().is_empty() {
                        return None;
                    }
                    let proof = TacticRecord {
                        solver_fen: capture.solver_fen,
                        solution: capture.solution,
                        provenance: vec![],
                        ..record.clone()
                    };
                    let verified = compile_tactics(&[proof]).ok()?;
                    let source = verified.items.get(&item.target)?;
                    (source.definition == item.definition).then_some(GameCapture {
                        capture_id: capture.capture_id,
                    })
                });
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
                        PackDifficulty::Easy => "easy",
                        PackDifficulty::Medium => "medium",
                        PackDifficulty::Hard => "hard",
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

    fn source_puzzle(
        source: &str,
        label: &str,
        membership: Option<(&str, usize)>,
    ) -> serde_json::Value {
        let metadata = membership.map(|(theme, position)| {
            format!(
                "tempo1:catalog-metadata:{}",
                json!({"Motif":theme,"Difficulty":"easy",
                "DeckId":format!("{theme}-easy-01"),"DeckPosition":position})
            )
        });
        let mut provenance = vec!["https://example.invalid/source".to_owned()];
        provenance.extend(metadata);
        json!({
            "namespace":"catalog-test","source_id":source,
            "solver_fen":"4k3/8/8/8/8/8/4P3/4K3 w - - 0 1","solution":["e2e4"],
            "label":label,"provenance":provenance,
        })
    }

    fn batch_from_puzzles(directory: &Path, puzzles: Vec<serde_json::Value>) -> Batch {
        let transfer = directory.join("transfer.json");
        std::fs::write(
            &transfer,
            json!({"schema_version":1,"puzzles":puzzles}).to_string(),
        )
        .unwrap();
        Batch::open(&transfer).unwrap()
    }

    fn batch(directory: &Path, label: &str, membership: Option<(&str, usize)>) -> Batch {
        batch_from_puzzles(directory, vec![source_puzzle("puzzle", label, membership)])
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
        assert!(
            before.catalog.items[target]
                .tactic_policy_facts
                .as_ref()
                .unwrap()
                .theme_ids()
                .is_empty()
        );
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
    fn legacy_tactic_metadata_is_persisted_for_new_aliases_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        let mut app = daily(path, "aliases.sqlite");
        let fresh = batch_from_puzzles(
            path,
            vec![
                source_puzzle("a", "First source", Some(("fork", 2))),
                source_puzzle("b", "Equivalent source", Some(("pin", 3))),
            ],
        );
        let report = apply(&mut app, fresh).unwrap();
        let target = report.sources[0].target.clone();
        assert_eq!(report.sources[1].target, target);
        assert!(report.sources[1].alias);
        drop(app);

        let mut app = super::super::open(&path.join("aliases.sqlite")).unwrap();
        let original = app.snapshot().unwrap();
        let item = &original.catalog.items[&target];
        assert_eq!(item.version, ContentVersion(1));
        assert_eq!(
            item.tactic_policy_facts.as_ref().unwrap().theme_ids(),
            &std::collections::BTreeSet::from(["fork".into(), "pin".into()]),
        );
        let later = batch_from_puzzles(
            path,
            vec![
                source_puzzle("a", "Relabelled source", Some(("skewer", 4))),
                source_puzzle("c", "New alias for retained target", Some(("mateIn1", 2))),
            ],
        );
        let report = apply(&mut app, later).unwrap();
        assert!(report.sources.iter().all(|source| source.target == target));
        assert_eq!(app.snapshot().unwrap(), original);
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
