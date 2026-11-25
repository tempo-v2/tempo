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
    tx.execute(
        "UPDATE prepared_queue SET rank_a=NULL,rank_b=NULL,scope_a=NULL,scope_b=NULL,slot_a=NULL,slot_b=NULL WHERE scope_a IS NOT NULL OR scope_b IS NOT NULL",
        [],
    )?;
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

pub fn active_bank(db: &Connection) -> Result<&'static str> {
    let value: i64 = db.query_row(
        "SELECT bank FROM forecast_order_state WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    Ok(if value == 0 { "a" } else { "b" })
}

#[derive(Clone, PartialEq)]
struct Placement {
    rank: Option<i64>,
    scope: Option<String>,
    slot: Option<i64>,
}

pub struct StagedOrder {
    inactive: &'static str,
    changes: Vec<(String, Placement)>,
}
impl StagedOrder {
    // Run under a consistent read snapshot, outside the shared writer.
    pub fn prepare(
        db: &Connection,
        report: &str,
        reach: &std::collections::HashMap<String, f64>,
    ) -> Result<Self> {
        ensure!(reach.len() <= 4096, "Forecast exceeds supported rank depth");
        ensure!(
            crate::queue_store::prepared(db)?,
            "Study is preparing. Retry after preparation completes."
        );
        let active = active_bank(db)?;
        let inactive = if active == "a" { "b" } else { "a" };
        let mut q=db.prepare(&format!("SELECT card_id,rank,rank_{active},scope_{active},slot_{active},rank_{inactive},scope_{inactive},slot_{inactive}
          FROM prepared_queue WHERE kind='opening' ORDER BY card_id"))?;
        let mut rows = q.query([])?;
        let mut placements = std::collections::BTreeMap::new();
        let mut next = 0;
        while let Some(row) = rows.next()? {
            let base: Option<i64> = row.get(1)?;
            let live = Placement {
                rank: row.get(2)?,
                scope: row.get(3)?,
                slot: row.get(4)?,
            };
            let target = Placement {
                rank: row.get(5)?,
                scope: row.get(6)?,
                slot: row.get(7)?,
            };
            next = next.max(live.rank.or(base).unwrap_or(0));
            placements.insert(row.get::<_, String>(0)?, (base, live, target));
        }
        let mut cards = vec![];
        for (id, (base, live, _)) in &placements {
            if let Some(probability) = reach.get(id) {
                let slot = live.rank.or(*base).unwrap_or_else(|| {
                    next += 1;
                    next
                });
                cards.push((id.clone(), slot, *probability));
            }
        }
        ensure!(
            cards.len() == reach.len(),
            "The forecast contains a retired card"
        );
        let mut slots: Vec<_> = cards.iter().map(|(_, slot, _)| *slot).collect();
        slots.sort_unstable();
        cards.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
        for (local, ((id, _, _), rank)) in cards.into_iter().zip(slots).enumerate() {
            placements.get_mut(&id).unwrap().1 = Placement {
                rank: Some(rank),
                scope: Some(report.into()),
                slot: Some(local as i64),
            };
        }
        let changes = placements
            .into_iter()
            .filter_map(|(id, (_, live, target))| (live != target).then_some((id, live)))
            .collect();
        Ok(Self { inactive, changes })
    }

    pub fn stage(self, writer: &crate::writer::Writer) -> Result<i64> {
        let bank = self.inactive;
        let sql = format!(
            "UPDATE prepared_queue SET rank_{bank}=?1,scope_{bank}=?2,slot_{bank}=?3 WHERE card_id=?4"
        );
        for chunk in self.changes.chunks(128) {
            let rows = chunk.to_vec();
            let update = sql.clone();
            writer.background(move |db| {
                let tx = db.transaction()?;
                {
                    let mut stmt = tx.prepare_cached(&update)?;
                    for (card, p) in &rows {
                        ensure!(
                            stmt.execute(params![p.rank, p.scope, p.slot, card])? == 1,
                            "Prepared card changed during ranking."
                        );
                    }
                }
                tx.commit()?;
                Ok(())
            })?;
        }
        Ok(i64::from(bank == "b"))
    }
}
