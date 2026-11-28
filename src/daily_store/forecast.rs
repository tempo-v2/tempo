//! The forecast coordinator's addressed Store operations.
use super::*;
use crate::daily_forecast::{Claim, Delivery};

struct Ticket {
    id: String,
    dataset: String,
    day: String,
    authoring: i64,
    profile: String,
    saved_order: Option<String>,
    expires: i64,
    digest: Option<String>,
    apply_expires: Option<i64>,
    outcome: Option<String>,
}

impl Ticket {
    fn load(db: &Connection, id: &str) -> Result<Option<Self>> {
        Ok(db.query_row(
            "SELECT ticket,dataset,day,authoring,profile,saved_order_day,expires,digest,apply_expires,outcome
             FROM forecast_claim WHERE ticket=?1",
            [id],
            |r| Ok(Self {
                id:r.get(0)?, dataset:r.get(1)?, day:r.get(2)?, authoring:r.get(3)?,
                profile:r.get(4)?, saved_order:r.get(5)?, expires:r.get(6)?,
                digest:r.get(7)?, apply_expires:r.get(8)?, outcome:r.get(9)?,
            }),
        ).optional()?)
    }

    fn current(&self, db: &Connection) -> Result<bool> {
        if day_at(Utc::now().timestamp())? != self.day {
            return Ok(false);
        }
        Ok(db
            .query_row(
                "SELECT current_day=?1 AND authoring=?2 AND last_order_day IS ?3
                AND (SELECT dataset FROM metadata)=?4 FROM daily_clock",
                (&self.day, self.authoring, &self.saved_order, &self.dataset),
                |r| r.get::<_, Option<bool>>(0),
            )?
            .unwrap_or(false))
    }

    fn applying(&self, db: &Connection, digest: &str) -> Result<bool> {
        Ok(self.current(db)?
            && self.digest.as_deref() == Some(digest)
            && self
                .apply_expires
                .is_some_and(|end| end > Utc::now().timestamp())
            && self.outcome.is_none())
    }

    fn finish(&self, db: &Connection, outcome: &str) -> Result<()> {
        db.execute(
            "UPDATE forecast_claim SET outcome=?2 WHERE ticket=?1",
            (&self.id, outcome),
        )?;
        db.execute(
            "UPDATE daily_day SET forecast_finished=1 WHERE day=?1",
            [&self.day],
        )?;
        Ok(())
    }
}

pub fn claim_today(
    path: &Path,
    writer: &Writer,
    profile: &str,
    budget: Instant,
) -> Result<Option<Claim>> {
    if profile != daily_forecast::PROFILE {
        return Ok(None);
    }
    let admitted = Utc::now().timestamp();
    let key = day_at(admitted)?;
    let db = capture_before(path, budget)?;
    let tx = db.unchecked_transaction()?;
    let Some((dataset, authoring, saved_order)) = tx
        .query_row(
            "SELECT (SELECT dataset FROM metadata), c.authoring, c.last_order_day
         FROM daily_day d CROSS JOIN daily_clock c
         WHERE d.day=?1 AND c.current_day=d.day AND d.mode='maia' AND d.fresh_order=0
           AND d.forecast_finished=0 AND NOT EXISTS(SELECT 1 FROM forecast_claim WHERE day=d.day)",
            [&key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    tx.commit()?;
    let expires = admitted + daily_forecast::BUDGET.as_secs() as i64;
    let id = format!("{:032x}", rand::random::<u128>());
    let ticket = Ticket {
        id,
        dataset,
        day: key,
        authoring,
        profile: profile.to_owned(),
        saved_order,
        expires,
        digest: None,
        apply_expires: None,
        outcome: None,
    };
    let claimed = writer.forecast(budget, move |db| {
        let tx = db.transaction_with_behavior(Immediate)?;
        if !ticket.current(&tx)? || Utc::now().timestamp() >= ticket.expires {
            return Ok(None);
        }
        let added = tx.execute(
            "INSERT INTO forecast_claim(day,ticket,dataset,authoring,profile,saved_order_day,expires)
             SELECT day,?2,?3,?4,?5,?6,?7 FROM daily_day
             WHERE day=?1 AND mode='maia' AND fresh_order=0 AND forecast_finished=0
             ON CONFLICT(day) DO NOTHING",
            params![ticket.day,ticket.id,ticket.dataset,ticket.authoring,ticket.profile,ticket.saved_order,ticket.expires],
        )?;
        tx.commit()?;
        Ok((added == 1).then_some(ticket))
    })?;
    let Some(ticket) = claimed else {
        return Ok(None);
    };

    let result = materialize_claim(path, &ticket, budget);
    if !matches!(result, Ok(Some(_))) {
        // The day stays claimed even when this optional status write cannot be admitted.
        let _ = writer.forecast(budget, move |db| {
            let tx = db.transaction_with_behavior(Immediate)?;
            ticket.finish(&tx, "without_rank")?;
            tx.commit()?;
            Ok(())
        });
    }
    result
}

fn materialize_claim(path: &Path, ticket: &Ticket, budget: Instant) -> Result<Option<Claim>> {
    let db = capture_before(path, budget)?;
    let tx = db.unchecked_transaction()?;
    let ready = |db: &Connection| {
        Ok::<_, anyhow::Error>(
            Instant::now() < budget
                && Utc::now().timestamp() < ticket.expires
                && ticket.current(db)?,
        )
    };
    if !ready(&tx)? {
        return Ok(None);
    }
    let Some(day) = day(&tx, &ticket.day)? else {
        return Ok(None);
    };
    let snapshot = authored_rows(&tx, &day, &clock(&tx)?, Projection::Forecast)?;
    tx.commit()?;
    let request = snapshot.compile(|| Instant::now() >= budget)?.request();
    daily_forecast::check_request_size(&request)?;
    let tx = db.unchecked_transaction()?;
    let current = ready(&tx)?;
    tx.commit()?;
    Ok(current.then(|| Claim {
        ticket: ticket.id.clone(),
        expires: ticket.expires,
        request,
    }))
}

pub fn submit(
    path: &Path,
    writer: &Writer,
    id: String,
    response: Response,
    received: i64,
) -> Result<Delivery> {
    let digest = blake3::hash(&serde_json::to_vec(&response)?)
        .to_hex()
        .to_string();
    let read_limit = Instant::now() + READ_BUDGET;
    let db = capture_before(path, read_limit)?;
    let tx = db.unchecked_transaction()?;
    let Some(ticket) = Ticket::load(&tx, &id)? else {
        return Ok(Delivery::Stale);
    };
    if ticket.digest.as_ref().is_some_and(|saved| saved != &digest) {
        return Ok(Delivery::Invalid);
    }
    if let Some(outcome) = ticket.outcome {
        return Ok(if outcome == "published" {
            Delivery::Published
        } else {
            Delivery::WithoutRank
        });
    }
    let now = Utc::now().timestamp();
    if !ticket.current(&tx)?
        || ticket
            .apply_expires
            .map_or(received >= ticket.expires, |end| now >= end)
    {
        return Ok(Delivery::Stale);
    }
    let remaining = ticket
        .apply_expires
        .unwrap_or(ticket.expires)
        .saturating_sub(now);
    let read_limit = read_limit.min(Instant::now() + Duration::from_secs(remaining.max(0) as u64));
    if Instant::now() >= read_limit {
        return Ok(Delivery::Stale);
    }
    limit_capture(&tx, read_limit);
    let Some(today) = day(&tx, &ticket.day)? else {
        return Ok(Delivery::Stale);
    };
    let Some(staged_offset) = staged_order(&tx, &ticket.day)? else {
        return Ok(Delivery::Stale);
    };
    let snapshot_clock = clock(&tx)?;
    let mut keys = Vec::new();
    let mut bytes = 0;
    {
        let mut query =
            tx.prepare("SELECT id FROM active_cards WHERE kind='opening' ORDER BY id")?;
        for row in query.query_map([], |row| row.get::<_, String>(0))? {
            let key = row?;
            bytes += key.len() + std::mem::size_of::<String>();
            ensure!(
                bytes <= CAPTURE_BYTES,
                "Forecast rank keys exceed their byte budget"
            );
            keys.push(key);
        }
    }
    let saved_order = saved_opening_order(&tx, &today, &snapshot_clock)?;
    tx.commit()?;
    if Instant::now() >= read_limit {
        return Ok(Delivery::Stale);
    }
    let basis = format!("{}:{}:{}", ticket.dataset, ticket.day, ticket.authoring);
    if response.profile != ticket.profile
        || response.basis != basis
        || (response.succeeded && response.verify(&basis, &keys).is_err())
        || (!response.succeeded && !response.scores.is_empty())
    {
        return Ok(Delivery::Invalid);
    }
    let now = Utc::now().timestamp();
    if ticket.apply_expires.is_none() && now >= ticket.expires {
        return Ok(Delivery::Stale);
    }
    let expiry = ticket.apply_expires.unwrap_or(now + 60);
    let Some(remaining) = expiry.checked_sub(now).filter(|remaining| *remaining > 0) else {
        return Ok(Delivery::Stale);
    };
    let deadline = Instant::now() + Duration::from_secs(remaining as u64);
    let no_rank = !response
        .scores
        .values()
        .any(|score| matches!(score, Score::Estimated { .. }));
    let claim_id = id.clone();
    let accepted_digest = digest.clone();
    let accepted = writer.forecast(deadline, move |db| {
        accept_result(db, &claim_id, &accepted_digest, expiry, received, no_rank)
    })?;
    if !accepted {
        return Ok(Delivery::Stale);
    }
    if no_rank {
        return Ok(Delivery::WithoutRank);
    }

    let ordered = order_openings(keys, &saved_order, Some(&response));
    let total = ordered.len();
    let mut offset = staged_offset;
    ensure!(offset <= total, "Forecast stage exceeds claimed rank");
    for keys in ordered[offset..].chunks(BATCH) {
        let keys = keys.to_vec();
        let id = id.clone();
        let digest = digest.clone();
        let staged = writer.forecast(deadline, move |db| stage(db, &id, &digest, offset, &keys))?;
        if !staged {
            return Ok(Delivery::Stale);
        }
        offset += BATCH.min(total - offset);
    }

    // Re-capture readiness and commitments after staging. A racing ordinary write
    // leaves the rank staged and lets the worker redeliver within the original lease.
    let db = capture_before(path, deadline)?;
    let tx = db.unchecked_transaction()?;
    let clock = clock(&tx)?;
    let Some(day) = day(&tx, &ticket.day)? else {
        return Ok(Delivery::Stale);
    };
    if !day.openings_ready(&clock) {
        return Ok(Delivery::Busy);
    }
    let overlay = if clock.priority == 0 {
        None
    } else {
        let basis = crate::opening_priority::Basis::current(&tx)?
            .context("Missing opening priority basis")?;
        let Some(prefix) = crate::opening_priority::published_prefix(&tx, &basis)? else {
            return Ok(Delivery::Busy);
        };
        let revision = crate::opening_priority::published_revision(&tx, &basis)?
            .context("Missing opening priority revision")?;
        Some((basis, prefix, revision))
    };
    let input = opening_rows(&tx, &day, &clock)?;
    tx.commit()?;
    let draft = input.compile(|| Instant::now() >= deadline)?.draft_until(
        clock.openings,
        &ordered,
        "maia",
        overlay
            .as_ref()
            .map_or(&[], |(_, prefix, _)| prefix.as_slice()),
        || Instant::now() >= deadline,
    );
    let Some(mut draft) = draft else {
        return Ok(Delivery::Busy);
    };
    if !draft.revoked_openings.is_empty() {
        return Ok(Delivery::Busy);
    }
    super::replace_with_staging_if_needed(
        super::Preparation::Forecast,
        writer,
        &day,
        &mut draft,
        deadline,
    )?;
    let overlay = overlay.map(|(basis, _, revision)| (basis, revision));
    writer.forecast(deadline, move |db| {
        let tx = db.transaction_with_behavior(Immediate)?;
        let Some(ticket) = Ticket::load(&tx, &id)? else {
            return Ok(Delivery::Stale);
        };
        if !ticket.applying(&tx, &digest)? {
            return Ok(Delivery::Stale);
        }
        if let Some((basis, revision)) = &overlay
            && (crate::opening_priority::Basis::current(&tx)?.as_ref() != Some(basis)
                || crate::opening_priority::published_revision(&tx, basis)? != Some(*revision))
        {
            return Ok(Delivery::Busy);
        }
        if !publish_fresh_order(&tx, &day, &draft, total)? {
            return Ok(Delivery::Busy);
        }
        ticket.finish(&tx, "published")?;
        tx.commit()?;
        Ok(Delivery::Published)
    })
}

fn staged_order(db: &Connection, key: &str) -> Result<Option<usize>> {
    Ok(db
        .query_row(
            "SELECT order_staged FROM daily_day WHERE day=?1",
            [key],
            |r| r.get(0),
        )
        .optional()?)
}

pub(super) fn publish_fresh_order(
    tx: &Connection,
    day: &Day,
    draft: &Draft,
    total: usize,
) -> Result<bool> {
    let ready: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM daily_day d CROSS JOIN daily_clock c
         WHERE d.day=?1 AND d.openings_version=c.openings AND d.mode='maia'
           AND d.fresh_order=0 AND d.order_staged=?2)",
        (&day.key, total),
        |r| r.get(0),
    )?;
    if !ready || !publish_slots(tx, day, draft)? {
        return Ok(false);
    }
    tx.execute(
        "UPDATE daily_day SET fresh_order=1,order_profile=?2 WHERE day=?1",
        (&day.key, daily_forecast::PROFILE),
    )?;
    tx.execute(
        "UPDATE daily_clock SET last_order_day=?1 WHERE last_order_day IS NULL OR last_order_day<=?1",
        [&day.key],
    )?;
    Ok(true)
}

fn accept_result(
    db: &mut Connection,
    id: &str,
    digest: &str,
    expiry: i64,
    received: i64,
    no_rank: bool,
) -> Result<bool> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let Some(ticket) = Ticket::load(&tx, id)? else {
        return Ok(false);
    };
    let now = Utc::now().timestamp();
    if !ticket.current(&tx)?
        || now >= expiry
        || ticket.apply_expires.is_some_and(|saved| saved != expiry)
        || (ticket.digest.is_none() && now >= ticket.expires)
    {
        return Ok(false);
    }
    let changed = tx.execute(
        "UPDATE forecast_claim SET digest=?2,apply_expires=?3 WHERE ticket=?1
         AND outcome IS NULL AND (digest=?2 OR (digest IS NULL AND expires>?4))",
        (id, digest, expiry, received),
    )?;
    if changed == 1 && no_rank {
        ticket.finish(&tx, "without_rank")?;
    }
    tx.commit()?;
    Ok(changed == 1)
}

fn stage(
    db: &mut Connection,
    id: &str,
    digest: &str,
    offset: usize,
    keys: &[String],
) -> Result<bool> {
    ensure!(
        keys.len() <= BATCH,
        "Forecast staging exceeds its row budget"
    );
    let tx = db.transaction_with_behavior(Immediate)?;
    let Some(ticket) = Ticket::load(&tx, id)? else {
        return Ok(false);
    };
    if !ticket.applying(&tx, digest)? || staged_order(&tx, &ticket.day)? != Some(offset) {
        return Ok(false);
    }
    write_rank_batch(&tx, &ticket.day, offset, keys)?;
    tx.commit()?;
    Ok(true)
}

#[cfg(test)]
pub(super) fn stage_first_rank(
    db: &mut Connection,
    id: &str,
    outcome: &Response,
    first: String,
) -> Result<()> {
    let digest = blake3::hash(&serde_json::to_vec(outcome)?)
        .to_hex()
        .to_string();
    let now = Utc::now().timestamp();
    ensure!(accept_result(db, id, &digest, now + 60, now, false)?);
    ensure!(stage(db, id, &digest, 0, &[first])?);
    Ok(())
}
