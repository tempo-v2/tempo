use super::{BATCH, Clock, Day, Openings, opening_rows, publish_slots};
use crate::{
    opening_priority::{self as goals, Basis, Goal, Graph, Proof, Resolution, Resolver, Update},
    writer::Writer,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Transaction, TransactionBehavior::Immediate};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

/// Disposable state under one coherent generation fence. The persisted prefix
/// lets optional rank publication reuse the selected dependencies.
pub(super) struct Pass {
    basis: Basis,
    input: Openings,
    pinned: HashSet<String>,
    cursor: i64,
    roots: VecDeque<Goal>,
    resolver: Resolver,
    prefix: Vec<Proof>,
    allocated: HashSet<String>,
    done: bool,
    published: Option<Vec<Proof>>,
    pending_updates: Vec<Update>,
    fresh_order: bool,
    schedule_floor: i64,
    base_order: Option<Vec<String>>,
}

impl Pass {
    fn new(
        basis: Basis,
        input: Openings,
        fresh_order: bool,
        schedule_floor: i64,
        published: Option<Vec<Proof>>,
    ) -> Self {
        let (pinned, _) = input.checked_commitments();
        Self {
            basis,
            input,
            allocated: pinned.clone(),
            pinned,
            cursor: 0,
            roots: VecDeque::new(),
            resolver: Resolver::default(),
            prefix: Vec::new(),
            done: false,
            published,
            pending_updates: Vec::new(),
            fresh_order,
            schedule_floor,
            base_order: None,
        }
    }

    fn base_order(&mut self, deadline: Instant) -> Option<&[String]> {
        if self.base_order.is_none() {
            // Match the saved ordering and canonical Standard tail. Check
            // the deadline between insertions, including large retained catalogs.
            let mut ordered = BTreeMap::new();
            for card in &self.input.authored.keys {
                if Instant::now() >= deadline {
                    return None;
                }
                let ordinal = self
                    .input
                    .saved_order
                    .get(card)
                    .copied()
                    .unwrap_or(usize::MAX);
                ordered.insert((ordinal, card), card.clone());
            }
            self.base_order = Some(ordered.into_values().collect());
        }
        self.base_order.as_deref()
    }

    fn advance(&mut self, deadline: Instant, zero_allowance: bool) -> Result<Vec<Update>> {
        let mut updates = Vec::new();
        while let Some(goal) = self.roots.front().cloned() {
            if Instant::now() >= deadline {
                break;
            }
            let update = if goal.witnessed {
                Update {
                    id: goal.id,
                    state: "fulfilled",
                    summary: json!({"remaining":0,"next":[]}),
                }
            } else if self.input.lifecycles.get(&goal.target) != Some(&goal.availability) {
                Update {
                    id: goal.id,
                    state: "invalidated",
                    summary: json!({"reason":"target_lifecycle_changed"}),
                }
            } else {
                let graph = Graph {
                    routes: &self.input.authored.projection.prerequisites,
                    availability: &self.input.lifecycles,
                    introduced: &self.input.introduced,
                };
                match self.resolver.resume(&goal.target, &graph, deadline) {
                    Resolution::More => break,
                    Resolution::Blocked(reason) => Update {
                        id: goal.id,
                        state: "blocked",
                        summary: json!({"reason":reason.code()}),
                    },
                    Resolution::Complete(proof) => {
                        self.observe(goal.id, &goal.target, proof, zero_allowance)?
                    }
                }
            };
            self.cursor = update.id;
            updates.push(update);
            self.resolver = Resolver::default();
            self.roots.pop_front();
        }
        Ok(updates)
    }

    fn observe(
        &mut self,
        id: i64,
        target: &str,
        proof: Vec<Proof>,
        zero_allowance: bool,
    ) -> Result<Update> {
        if proof.is_empty() {
            return Ok(Update {
                id,
                state: "fulfilled",
                summary: json!({"remaining":0,"next":[]}),
            });
        }
        let mut hash = blake3::Hasher::new();
        serde_json::to_writer(&mut hash, &proof)?;
        let next: Vec<_> = proof.iter().take(20).map(|entry| json!({
            "card_id":entry.card,"role":if entry.card == target { "target" } else { "parent" }
        })).collect();
        let first = &proof[0].card;
        let reason = if self.basis.paused {
            "opening_paused"
        } else if zero_allowance {
            "zero_daily_allowance"
        } else if self.input.limit == 0 {
            "allowance_exhausted"
        } else if self.input.schedule_held.contains(first) {
            "waiting_next_day_schedule_cutoff"
        } else if self.pinned.contains(first) {
            "already_active_or_committed"
        } else if first != target {
            "awaiting_parents"
        } else {
            "awaiting_study"
        };
        let remaining = proof.len();
        for entry in proof {
            if self.prefix.len() == self.input.limit {
                break;
            }
            if !self.allocated.contains(&entry.card)
                && self.input.priority_available.contains_key(&entry.card)
                && entry
                    .parents
                    .iter()
                    .all(|parent| self.allocated.contains(parent))
            {
                self.allocated.insert(entry.card.clone());
                self.prefix.push(entry);
            }
        }
        Ok(Update {
            id,
            state: "pending",
            summary: json!({
                "remaining":remaining,"next":next,"prefix":remaining>next.len(),
                "proof_fingerprint":hash.finalize().to_hex().to_string(),"reason":reason,
                "completion_basis": (remaining<=20).then(||
                    json!({"authoring":self.basis.authoring,
                        "introduction_reset":self.basis.introduction_reset,
                        "schedule_floor":self.schedule_floor})),
            }),
        })
    }
}

pub(super) fn turn(
    tx: Transaction<'_>,
    writer: &Writer,
    day: Day,
    clock: Clock,
    active: &mut Option<Pass>,
    read_deadline: Instant,
) -> Result<bool> {
    let basis = Basis::current(&tx)?.context("Missing opening priority basis")?;
    let reuse = active
        .as_ref()
        .is_some_and(|pass| pass.basis == basis && pass.fresh_order == day.fresh);
    if reuse && active.as_ref().is_some_and(|pass| pass.done) {
        return Ok(false);
    }
    let unfinished = reuse
        && active
            .as_ref()
            .is_some_and(|pass| !pass.roots.is_empty() || !pass.pending_updates.is_empty());
    let page = if unfinished {
        None
    } else {
        Some(goals::roots(
            &tx,
            if reuse {
                active.as_ref().unwrap().cursor
            } else {
                0
            },
        )?)
    };
    let capture = if reuse {
        None
    } else {
        Some((
            opening_rows(&tx, &day, &clock)?,
            goals::published_prefix(&tx, &basis)?,
        ))
    };
    tx.commit()?;

    if let Some((rows, published)) = capture {
        *active = Some(Pass::new(
            basis.clone(),
            rows.compile(|| Instant::now() >= read_deadline)?,
            day.fresh,
            clock.sequence,
            published,
        ));
    }
    let pass = active.as_mut().unwrap();
    let exhausted = page.as_ref().is_some_and(Vec::is_empty);
    if let Some(page) = page {
        pass.roots = page.into();
    }
    if pass.pending_updates.is_empty() {
        pass.pending_updates = pass.advance(read_deadline, day.opening_limit == 0)?;
    }
    if Instant::now() >= read_deadline {
        return Ok(true);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    // Save the completion proof before Study can consume newly published slots.
    if !pass.pending_updates.is_empty() {
        let updates = pass.pending_updates.clone();
        ensure!(
            updates.len() <= BATCH,
            "too many opening priority updates in a turn"
        );
        let status_basis = basis.clone();
        if !writer.background_until(deadline, move |db| {
            let tx = db.transaction_with_behavior(Immediate)?;
            if !goals::publish_updates(&tx, &status_basis, &updates)? {
                return Ok(false);
            }
            tx.commit()?;
            Ok(true)
        })? {
            *active = None;
            return Ok(true);
        }
        pass.pending_updates.clear();
    }
    // Reconstructing after restart or a fresh rank must never replace a valid
    // later-page prefix with a shorter locally recomputed prefix.
    if pass
        .published
        .as_ref()
        .is_none_or(|old| old.len() < pass.prefix.len())
    {
        if pass.base_order(read_deadline).is_none() {
            return Ok(true);
        }
        let source = if day.fresh {
            "maia"
        } else if pass.input.saved_order.is_empty() {
            "standard"
        } else {
            "saved"
        };
        let Some(draft) = pass.input.draft_until(
            clock.openings,
            pass.base_order.as_deref().unwrap(),
            source,
            &pass.prefix,
            || Instant::now() >= read_deadline,
        ) else {
            return Ok(true);
        };
        if Instant::now() >= read_deadline {
            return Ok(true);
        }
        let published_basis = basis.clone();
        let prefix = pass.prefix.clone();
        if !writer.background_until(deadline, move |db| {
            let tx = db.transaction_with_behavior(Immediate)?;
            if super::day_at(chrono::Utc::now().timestamp())? != day.key
                || Basis::current(&tx)?.as_ref() != Some(&published_basis)
                || super::day(&tx, &day.key)?.is_none_or(|current| current.fresh != day.fresh)
            {
                return Ok(false);
            }
            let before = tx.total_changes();
            if !publish_slots(&tx, &day, &draft)? {
                return Ok(false);
            }
            goals::publish_prefix(&tx, &published_basis, &prefix)?;
            ensure!(
                tx.total_changes() - before <= 128,
                "priority opening publication exceeds write bound"
            );
            tx.commit()?;
            Ok(true)
        })? {
            *active = None;
            return Ok(true);
        }
        pass.published = Some(pass.prefix.clone());
    }
    pass.done = exhausted;
    Ok(true)
}
