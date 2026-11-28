use super::{
    BATCH, Clock, Day, Draft, Openings, Preparation, allocate_stage, opening_rows, publish_slots,
    replace_with_staging_if_needed,
};
use crate::{
    opening_priority::{self as goals, Basis, Goal, Graph, Proof, Resolution, Resolver, Update},
    writer::Writer,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, Transaction, TransactionBehavior::Immediate};
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
    published_through: i64,
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
        published_through: i64,
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
            published_through,
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
                    completion_cards: None,
                }
            } else if self.input.lifecycles.get(&goal.target) != Some(&goal.availability) {
                Update {
                    id: goal.id,
                    state: "invalidated",
                    summary: json!({"reason":"target_lifecycle_changed"}),
                    completion_cards: None,
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
                        completion_cards: None,
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
                completion_cards: None,
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
        let completion_cards = (remaining > next.len())
            .then(|| {
                let cards: Vec<_> = proof.iter().map(|entry| &entry.card).collect();
                serde_json::to_vec(&cards)
            })
            .transpose()?;
        ensure!(
            completion_cards
                .as_ref()
                .is_none_or(|data| data.len() <= 512 * 1024),
            "Opening priority witness exceeds its byte budget"
        );
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
                "completion_basis": {"authoring":self.basis.authoring,
                    "introduction_reset":self.basis.introduction_reset,
                    "schedule_floor":self.schedule_floor},
            }),
            completion_cards,
        })
    }
}

/// A progress refresh can change the witness without changing any slot in
/// the current lane. Preserve that lane if its exact ordered dependencies agree.
fn same_opening_slots(
    db: &Connection,
    day: &Day,
    clock: &Clock,
    draft: &Draft,
    introduced: &HashSet<String>,
    deadline: Instant,
) -> Result<bool> {
    if day.opening_head == 0
        || !day.openings_ready(clock)
        || draft.source != day.source
        || !draft.revoked_openings.is_empty()
    {
        return Ok(false);
    }
    let mut query = db.prepare_cached(
        "SELECT card_id,availability_version,parents FROM daily_new
         WHERE day=?1 AND generation=?2 AND kind='opening' AND committed=0 ORDER BY ordinal",
    )?;
    let mut existing = query.query((&day.key, day.opening_head))?;
    for (selected, availability) in &draft.selected {
        ensure!(
            Instant::now() < deadline,
            "Opening priority preparation timed out"
        );
        let Some(row) = existing.next()? else {
            return Ok(false);
        };
        let card: String = row.get(0)?;
        let version: i64 = row.get(1)?;
        if card != selected.card || version != *availability {
            return Ok(false);
        }
        let parents = serde_json::from_str::<Vec<String>>(&row.get::<_, String>(2)?)?;
        let pending: HashSet<_> = parents
            .iter()
            .filter(|parent| !introduced.contains(*parent))
            .collect();
        if pending.len() != selected.waiting_on.len()
            || selected
                .waiting_on
                .iter()
                .any(|parent| !pending.contains(parent))
        {
            return Ok(false);
        }
    }
    Ok(existing.next()?.is_none())
}

pub(super) fn turn(
    db: &Connection,
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
            goals::published_through(&tx, &basis)?.unwrap_or_default(),
        ))
    };
    tx.commit()?;

    if let Some((rows, published, through)) = capture {
        *active = Some(Pass::new(
            basis.clone(),
            rows.compile(|| Instant::now() >= read_deadline)?,
            day.fresh,
            clock.sequence,
            published,
            through,
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
    if !save_updates(writer, &basis, &pass.pending_updates, deadline)? {
        *active = None;
        return Ok(true);
    }
    pass.pending_updates.clear();
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
        let Some(mut draft) = pass.input.draft_until(
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
        let unchanged = same_opening_slots(
            db,
            &day,
            &clock,
            &draft,
            &pass.input.introduced,
            read_deadline,
        )?;
        if !unchanged {
            replace_with_staging_if_needed(
                Preparation::Required,
                writer,
                &day,
                &mut draft,
                read_deadline,
            )?;
        }
        let (inline, generation) =
            prepare_prefix(writer, &day, draft.staged, &pass.prefix, read_deadline)?;
        let draft_version = draft.version;
        let draft = (!unchanged).then_some(draft);
        let published_basis = basis.clone();
        let through = pass.cursor.max(pass.published_through);
        if !writer.background_until(deadline, move |db| {
            let tx = db.transaction_with_behavior(Immediate)?;
            if super::day_at(chrono::Utc::now().timestamp())? != day.key
                || Basis::current(&tx)?.as_ref() != Some(&published_basis)
                || super::day(&tx, &day.key)?.is_none_or(|current| {
                    current.fresh != day.fresh
                        || current.opening_head != day.opening_head
                        || unchanged && current.openings != Some(draft_version)
                })
            {
                return Ok(false);
            }
            let before = tx.total_changes();
            if let Some(draft) = &draft
                && !publish_slots(&tx, &day, draft)?
            {
                return Ok(false);
            }
            goals::publish_prefix(&tx, &published_basis, &inline, generation, through)?;
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
        pass.published_through = through;
    } else if pass.cursor > pass.published_through {
        let through = pass.cursor;
        if !writer.background_until(deadline, move |db| {
            let tx = db.transaction_with_behavior(Immediate)?;
            if Basis::current(&tx)?.as_ref() != Some(&basis) {
                return Ok(false);
            }
            goals::advance_projection(&tx, through)?;
            tx.commit()?;
            Ok(true)
        })? {
            *active = None;
            return Ok(true);
        }
        pass.published_through = through;
    }
    pass.done = exhausted;
    Ok(true)
}

pub(super) fn save_updates(
    writer: &Writer,
    basis: &Basis,
    updates: &[Update],
    deadline: Instant,
) -> Result<bool> {
    let mut remaining = updates;
    while !remaining.is_empty() {
        let mut bytes = 0;
        let count = remaining
            .iter()
            .take(BATCH)
            .take_while(|update| {
                bytes += 15 * 1024 + update.completion_cards.as_ref().map_or(0, Vec::len);
                bytes <= 544 * 1024
            })
            .count();
        ensure!(
            count > 0,
            "Opening priority witness exceeds its transaction budget"
        );
        let batch = remaining[..count].to_vec();
        let basis = basis.clone();
        if !writer.background_until(deadline, move |db| {
            let tx = db.transaction_with_behavior(Immediate)?;
            if super::day_at(chrono::Utc::now().timestamp())? != basis.day
                || !goals::publish_updates(&tx, &basis, &batch)?
            {
                return Ok(false);
            }
            tx.commit()?;
            Ok(true)
        })? {
            return Ok(false);
        }
        remaining = &remaining[count..];
    }
    Ok(true)
}

pub(super) fn prepare_prefix(
    writer: &Writer,
    day: &Day,
    staged: Option<i64>,
    prefix: &[Proof],
    deadline: Instant,
) -> Result<(String, i64)> {
    let payload = serde_json::to_vec(prefix)?;
    ensure!(
        payload.len() <= super::CAPTURE_BYTES,
        "Opening priority prefix exceeds its byte budget"
    );
    if payload.len() <= 32 * 1024 {
        return Ok((String::from_utf8(payload)?, 0));
    }
    let generation = match staged {
        Some(generation) => generation,
        None => allocate_stage(Preparation::Required, writer, day, "opening", deadline)?,
    };
    for (ordinal, chunk) in payload.chunks(32 * 1024).enumerate() {
        let chunk = chunk.to_vec();
        writer.background_until(deadline, move |db| {
            db.execute(
                "INSERT INTO daily_new_prefix VALUES(?1,?2,?3)",
                (generation, ordinal, &chunk),
            )?;
            Ok(())
        })?;
    }
    Ok(("[]".into(), generation))
}

pub(super) struct Extension {
    pub draft: Draft,
    pub prefix: Vec<Proof>,
    pub through: i64,
    pub basis: Basis,
    pub updates: Vec<Update>,
}

/// Resolve the same priority prefix against the proposed ceiling before settling
/// an explicit addition. Ordinary ordering fills the space left by that prefix.
pub(super) fn extended_draft(
    db: &rusqlite::Connection,
    day: &Day,
    clock: &Clock,
    deadline: Instant,
) -> Result<Extension> {
    let input = opening_rows(db, day, clock)?.compile(|| Instant::now() >= deadline)?;
    let basis = Basis::current(db)?.context("Missing opening priority basis")?;
    let mut pass = Pass::new(basis.clone(), input, day.fresh, clock.sequence, None, 0);
    let mut updates = Vec::new();
    let mut update_bytes = 0;
    while clock.priority != 0 && pass.prefix.len() < pass.input.limit {
        ensure!(
            Instant::now() < deadline,
            "Opening priority preparation timed out"
        );
        pass.roots = goals::roots(db, pass.cursor)?.into();
        if pass.roots.is_empty() {
            break;
        }
        for update in pass.advance(deadline, false)? {
            update_bytes += serde_json::to_vec(&update.summary)?.len()
                + update.completion_cards.as_ref().map_or(0, Vec::len);
            ensure!(
                update_bytes <= super::CAPTURE_BYTES,
                "Opening priority evidence exceeds its byte budget"
            );
            updates.push(update);
        }
        ensure!(
            pass.roots.is_empty(),
            "Opening priority preparation timed out"
        );
    }
    let source = if day.fresh {
        "maia"
    } else if pass.input.saved_order.is_empty() {
        "standard"
    } else {
        "saved"
    };
    let order = pass.input.ordered(None);
    let draft = pass
        .input
        .draft_until(clock.openings, &order, source, &pass.prefix, || {
            Instant::now() >= deadline
        })
        .context("Opening introduction preparation timed out")?;
    Ok(Extension {
        draft,
        prefix: pass.prefix,
        through: pass.cursor,
        basis,
        updates,
    })
}
