//! Preview and apply the one-time opening partition through the catalog boundary.
use super::{LocalClock, LocalResult, Profile, stored_profile};
use crate::Clock;
use crate::game_model::model_input_stamp;
use serde_json::json;
use std::{collections::BTreeMap, path::Path};
use tempo_chess::{
    AuthoredRepertoire, ChangeKind, ContentKind, LearnerColor, compile_repertoire,
    partition_black_openings, plan_repertoire_replacement,
};
use tempo_store::{
    Commit, ContentReceipt, ContentSource, ModelAuthority, RepertoirePartition, SqliteStore, Store,
    Write, model_targets,
};

/// Preview without writing, or revalidate and commit a previously printed token.
pub fn split_black_repertoire(path: &Path, id: &str, apply: Option<&str>) -> LocalResult<()> {
    let mut store = if apply.is_some() {
        SqliteStore::open(path)?
    } else {
        SqliteStore::open_read_only(path)?
    };
    let snapshot = store.content_state()?;
    if let Some(token) = apply {
        let request = receipt_id(&snapshot.dataset.0, id, token);
        if let Some(saved) = store.content_receipt(&request)? {
            let saved: serde_json::Value = serde_json::from_str(&saved)?;
            println!("{}", serde_json::to_string_pretty(&saved["summary"])?);
            return Ok(());
        }
    }

    let document = store.repertoire_document(id)?;
    if document.dataset != snapshot.dataset || document.revision != snapshot.content_revision {
        return Err("The source changed while reading it. Preview again.".into());
    }
    if document.suspended || document.model_authority != ModelAuthority::Verified {
        return Err("Choose an active repertoire with a verified source document.".into());
    }
    let item = document.item.ok_or("Unknown source repertoire")?;
    if item.definition.learner_color()? != LearnerColor::Black {
        return Err("This split requires a Black repertoire.".into());
    }
    let original = document.pgn.ok_or("The source has no PGN")?;
    let token = blake3::hash(&serde_json::to_vec(&(
        "black-opening-partition-v1",
        &snapshot.dataset,
        id,
        snapshot.content_revision,
        &original,
    ))?)
    .to_hex()
    .to_string();
    if apply.is_some_and(|expected| expected != token) {
        return Err("The split preview is outdated. Preview again.".into());
    }
    let slav = format!(
        "repertoire-{}",
        &blake3::hash(format!("{id}:split:d4").as_bytes()).to_hex()[..32]
    );
    let other = format!(
        "repertoire-{}",
        &blake3::hash(format!("{id}:split:other").as_bytes()).to_hex()[..32]
    );
    let names = [
        item.label.as_str(),
        "Slav / vs. 1.d4",
        "Black vs. Other First Moves",
    ];
    let ids = [id, slav.as_str(), other.as_str()];
    let groups = partition_black_openings(&original)?;
    let original_targets: BTreeMap<_, _> = snapshot
        .catalog
        .items
        .values()
        .filter_map(|item| match &item.definition.kind {
            ContentKind::Repertoire {
                repertoire,
                position,
            } if repertoire == id && item.active() => Some((position, item)),
            _ => None,
        })
        .collect();

    let profile: Profile = serde_json::from_str(&stored_profile(&store)?)?;
    let now = LocalClock(profile.study_timezone.parse()?).now();
    let mut same_day_introductions = 0;
    let mut catalog = snapshot.catalog.clone();
    let mut writes = Vec::new();
    let mut mapping = BTreeMap::new();
    let mut stamps = Vec::new();
    let mut sources = Vec::new();
    let mut summaries = Vec::new();
    let mut exceptions = 0;
    for (index, pgn) in groups.iter().enumerate() {
        let id = ids[index];
        if index > 0 && catalog.items.values().any(|item| {
            matches!(&item.definition.kind, ContentKind::Repertoire { repertoire, .. } if repertoire == id)
        }) {
            return Err("A destination repertoire already exists. The source was left intact.".into());
        }
        let authored = AuthoredRepertoire::parse(pgn)?;
        if index == 0 {
            exceptions = authored
                .routes
                .iter()
                .filter(|route| route.get(1).is_none_or(|(_, uci)| uci != "c7c6"))
                .count();
        }
        let source = format!("pgn:{}", blake3::hash(pgn.as_bytes()).to_hex());
        let incoming = compile_repertoire(pgn, id, names[index], LearnerColor::Black, &source)?;
        let plan = plan_repertoire_replacement(&catalog, &incoming, id)?;
        if plan
            .changes
            .iter()
            .any(|change| change.kind == ChangeKind::Semantic)
        {
            return Err("Partition would change an existing prepared answer.".into());
        }
        catalog = plan.catalog;
        for target in incoming.items.keys() {
            let item = catalog.items.get_mut(target).expect("planned item");
            let ContentKind::Repertoire { position, .. } = &item.definition.kind else {
                unreachable!("compiled repertoire item");
            };
            let old = original_targets
                .get(position)
                .ok_or("Partition introduced an unknown position")?;
            if old.definition.decisions != item.definition.decisions
                || old.definition.scheme != item.definition.scheme
            {
                return Err("Partition changed the prepared reply for a position.".into());
            }
            if index > 0 {
                // A saved Learning obligation is bound to an answer version.
                item.version = old.version;
                let state = snapshot
                    .learning
                    .get(&old.target)
                    .ok_or("Source Learning state is missing")?;
                if state.first_introduced == Some(now.day) && !state.game_introduction {
                    same_day_introductions += 1;
                }
                writes.push(Write::Learning {
                    target: target.clone(),
                    state: state.clone(),
                });
                mapping.insert(target.clone(), old.target.clone());
            }
        }
        stamps.push(model_input_stamp(
            &authored,
            LearnerColor::Black,
            model_targets(&catalog, id),
        ));
        sources.push(source);
        summaries.push(json!({
            "id": id, "name": names[index], "routes": authored.routes.len(),
            "active_targets": incoming.items.len()
        }));
    }
    let blocked = snapshot.active_attempt.as_ref().is_some_and(|active| {
        let attempt = &snapshot.attempts[active].attempt;
        catalog
            .items
            .get(&attempt.target)
            .is_none_or(|item| !item.active() || item.version != attempt.content_version)
    });
    let summary = json!({
        "status": if apply.is_some() { "applied" } else { "preview" },
        "token": token, "repertoires": summaries,
        "e4_routes_without_c6": exceptions,
        "copied_learning": mapping.len(),
        "blocked_by_active_attempt": blocked,
        "same_day_introductions": same_day_introductions,
    });
    if apply.is_some() {
        if blocked {
            return Err("Finish the current Study attempt before applying this split.".into());
        }
        if same_day_introductions > 0 {
            return Err(
                "Apply after the next study day starts to preserve today's introduction allowance."
                    .into(),
            );
        }
        writes.insert(0, Write::Catalog(catalog));
        let documents = ids
            .iter()
            .enumerate()
            .map(|(index, repertoire_id)| ContentSource::RepertoireDocument {
                id: &sources[index],
                pgn: &groups[index],
                repertoire_id,
                expected_content_revision: snapshot.content_revision,
                model_input: &stamps[index],
                forecast: None,
            })
            .collect();
        let request = receipt_id(&snapshot.dataset.0, id, &token);
        let reply = serde_json::to_string(&json!({"summary": summary}))?;
        store.commit_repertoire_partition(
            Commit {
                expected_revision: snapshot.revision,
                expected_dataset: snapshot.dataset,
                writes,
            },
            documents,
            ContentReceipt {
                request: &request,
                reply: &reply,
            },
            RepertoirePartition {
                source_repertoire: id,
                destinations: &ids[1..],
                source_by_destination: &mapping,
                applied_at_ms: now.unix_ms,
            },
        )?;
    }
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

fn receipt_id(dataset: &str, repertoire: &str, token: &str) -> String {
    format!("opening-partition-v1:{dataset}:{repertoire}:{token}")
}
