// Offline rankers consume this catalogue snapshot and return only a ranked list of IDs.
use crate::{chess, db};
use anyhow::{Result, ensure};
use rusqlite::{Connection, TransactionBehavior::Immediate, params};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Serialize)]
pub struct RankInput {
    dataset: String,
    revision: String,
    routes: Vec<RankRoute>,
    cards: Vec<RankCard>,
}

#[derive(Serialize)]
struct RankRoute {
    repertoire_id: String,
    color: String,
    route: chess::Route,
    start_at: usize,
}

#[derive(Serialize)]
struct RankCard {
    id: String,
    repertoire_id: String,
    color: String,
    position_id: String,
    fen: String,
    answers: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankSnapshot {
    dataset: String,
    revision: String,
    card_ids: Vec<String>,
}

pub fn read(db: &Connection) -> Result<RankInput> {
    let mut q =
        db.prepare("SELECT repertoire_id,color,route,start_at FROM openings ORDER BY id")?;
    let mut rows = q.query([])?;
    let mut routes = Vec::new();
    while let Some(row) = rows.next()? {
        routes.push(RankRoute {
            repertoire_id: row.get(0)?,
            color: row.get(1)?,
            route: serde_json::from_str(row.get_ref(2)?.as_str()?)?,
            start_at: row.get(3)?,
        });
    }
    let mut q = db.prepare("SELECT id,repertoire_id,color,position_id,fen,answers FROM cards WHERE kind='opening' ORDER BY id")?;
    let mut rows = q.query([])?;
    let mut cards = Vec::new();
    while let Some(row) = rows.next()? {
        cards.push(RankCard {
            id: row.get(0)?,
            repertoire_id: row.get(1)?,
            color: row.get(2)?,
            position_id: row.get(3)?,
            fen: row.get(4)?,
            answers: serde_json::from_str(row.get_ref(5)?.as_str()?)?,
        });
    }
    let revision = chess::id(&[
        "opening-rank-input-v1",
        &serde_json::to_string(&routes)?,
        &serde_json::to_string(&cards)?,
    ]);
    Ok(RankInput {
        dataset: db::dataset(db)?,
        revision,
        routes,
        cards,
    })
}

pub fn replace(db: &mut Connection, snapshot: RankSnapshot) -> Result<()> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let current = read(&tx)?;
    ensure!(
        snapshot.dataset == current.dataset,
        "opening order belongs to a different dataset"
    );
    ensure!(
        snapshot.revision == current.revision,
        "opening catalogue changed since the rank input was read"
    );
    let mut valid: HashSet<_> = current.cards.iter().map(|card| card.id.as_str()).collect();
    for id in &snapshot.card_ids {
        ensure!(
            valid.remove(id.as_str()),
            "opening order contains an unknown or duplicate card"
        );
    }
    tx.execute("DELETE FROM opening_order", [])?;
    {
        let mut insert = tx.prepare("INSERT INTO opening_order(rank,card_id) VALUES (?1,?2)")?;
        for (i, id) in snapshot.card_ids.iter().enumerate() {
            insert.execute(params![i + 1, id])?;
        }
    }
    tx.commit()?;
    Ok(())
}
