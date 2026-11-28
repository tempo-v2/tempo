//! Receipt-first acceptance of explicitly selected opening misses.
use crate::{
    catalog::{self, MissTarget},
    db,
    game_feedback::{self, MissBasis, Pair},
    games, review, study_store,
};
use anyhow::{Result, ensure};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior::Immediate, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const CONTRACT: u8 = 1;
const WRITE_LIMIT: u64 = 128;
const VM_LIMIT: u64 = 2_000_000;

#[derive(Debug)]
pub enum Rejected {
    Invalid,
    Dataset,
    Conflict,
    TargetChanged,
    AuthoringChanged,
    EligibilityChanged,
    LearningChanged,
    ActiveStudy,
    Missing,
}

impl Rejected {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid => "invalid_opening_miss",
            Self::Dataset => "stale_dataset",
            Self::Conflict => "idempotency_conflict",
            Self::TargetChanged => "target_changed",
            Self::AuthoringChanged => "authoring_changed",
            Self::EligibilityChanged => "eligibility_changed",
            Self::LearningChanged => "learning_changed",
            Self::ActiveStudy => "target_in_active_study",
            Self::Missing => "opening_miss_not_found",
        }
    }
}
impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "Invalid opening miss request or unsupported contract version.",
            Self::Dataset => "This request addresses a different Tempo dataset.",
            Self::Conflict => "This request ID is bound to a different payload.",
            Self::TargetChanged => {
                "The exact active opening target changed or is unavailable. Prepare again."
            }
            Self::AuthoringChanged => "The repertoire changed. Prepare the unresolved miss again.",
            Self::EligibilityChanged => "The target's introduction state changed. Prepare again.",
            Self::LearningChanged => "Learning on this target changed. Prepare again.",
            Self::ActiveStudy => {
                "This target is awaiting its Study answer. Resolve it and prepare again."
            }
            Self::Missing => "No opening miss receipt or priority goal matches that identity.",
        })
    }
}
impl std::error::Error for Rejected {}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    Manual {
        event_id: String,
    },
    ArchivedGame {
        game_id: String,
        repertoire_id: String,
        decision_plies: Vec<usize>,
        report_generation: i64,
        comparator_version: i64,
        repertoire_revision: i64,
    },
}

struct Archived<'a> {
    pair: Pair,
    plies: &'a [usize],
    basis: MissBasis,
}

impl Source {
    fn archived(&self) -> Option<Archived<'_>> {
        match self {
            Self::Manual { .. } => None,
            Self::ArchivedGame {
                game_id,
                repertoire_id,
                decision_plies,
                report_generation,
                comparator_version,
                repertoire_revision,
            } => Some(Archived {
                pair: Pair {
                    game: game_id.clone(),
                    repertoire: repertoire_id.clone(),
                },
                plies: decision_plies,
                basis: MissBasis {
                    report_generation: *report_generation,
                    comparator_version: *comparator_version,
                    repertoire_revision: *repertoire_revision,
                },
            }),
        }
    }

    fn validate(&self) -> Result<()> {
        let valid = match self {
            Self::Manual { event_id } => uuid(event_id),
            Self::ArchivedGame {
                game_id,
                repertoire_id,
                decision_plies,
                report_generation,
                comparator_version,
                repertoire_revision,
            } => {
                id(game_id)
                    && id(repertoire_id)
                    && !decision_plies.is_empty()
                    && decision_plies.len() <= 512
                    && decision_plies
                        .iter()
                        .all(|&ply| ply < crate::chess::MAX_PLIES)
                    && decision_plies.windows(2).all(|p| p[0] < p[1])
                    && *report_generation >= 0
                    && *comparator_version > 0
                    && *repertoire_revision >= 0
            }
        };
        ensure!(valid, Rejected::Invalid);
        Ok(())
    }

    fn event(&self, dataset: &str, position: &str) -> String {
        match self {
            Self::Manual { event_id } => json!([dataset, "manual", event_id.to_ascii_lowercase()]),
            Self::ArchivedGame {
                game_id,
                repertoire_id,
                ..
            } => json!([dataset, "archived_game", game_id, repertoire_id, position]),
        }
        .to_string()
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    note: Option<String>,
    source_url: Option<String>,
    source_date: Option<String>,
    source_time: Option<String>,
    precision: Option<String>,
    origin: Option<String>,
}

impl Provenance {
    fn validate(&self) -> Result<()> {
        let within = |value: &Option<String>, max| value.as_ref().is_none_or(|s| s.len() <= max);
        ensure!(
            within(&self.note, 1024)
                && within(&self.source_url, 2048)
                && within(&self.source_date, 64)
                && within(&self.source_time, 64)
                && within(&self.precision, 32)
                && within(&self.origin, 64),
            Rejected::Invalid
        );
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preview {
    pub dataset: String,
    pub request_id: String,
    pub source: Source,
    pub card_id: Option<String>,
    pub position_id: Option<String>,
    #[serde(default)]
    pub provenance: Provenance,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum LearningState {
    Introduced,
    Unseen,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    availability_version: i64,
    reply_revision: i64,
    authoring_revision: i64,
    learning_state: LearningState,
    schedule_version: i64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    contract_version: u8,
    dataset: String,
    request_id: String,
    source: Source,
    target: MissTarget,
    expected: Expected,
    #[serde(default)]
    provenance: Provenance,
}

#[derive(Deserialize)]
pub struct DatasetQuery {
    pub dataset: String,
}

fn id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

fn uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, byte)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn check_dataset(db: &Connection, dataset: &str) -> Result<()> {
    ensure!(id(dataset), Rejected::Invalid);
    ensure!(db::dataset(db)? == dataset, Rejected::Dataset);
    Ok(())
}

struct Delivery {
    original: String,
    receipt: Value,
    fingerprint: Option<String>,
}

impl Delivery {
    fn wrap(&self, request: &str, accepted: bool) -> Value {
        let delivery = if accepted {
            "accepted"
        } else if self.original == request {
            "replayed"
        } else {
            "already_processed"
        };
        json!({"delivery":delivery,"request_id":request,"receipt":self.receipt})
    }
}

fn read_delivery(db: &Connection, request: &str) -> Result<Option<Delivery>> {
    let row: Option<(String, String, String)> = db
        .query_row(
            "SELECT r.request, r.receipt, q.fingerprint FROM opening_miss_requests q
         JOIN opening_miss_receipts r ON r.request=q.original WHERE q.request=?1",
            [request],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    row.map(|(original, receipt, fingerprint)| {
        Ok(Delivery {
            original,
            receipt: serde_json::from_str(&receipt)?,
            fingerprint: Some(fingerprint),
        })
    })
    .transpose()
}

fn read_event(db: &Connection, event: &str) -> Result<Option<Delivery>> {
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT request,receipt FROM opening_miss_receipts WHERE event=?1",
            [event],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(original, receipt)| {
        Ok(Delivery {
            original,
            receipt: serde_json::from_str(&receipt)?,
            fingerprint: None,
        })
    })
    .transpose()
}

pub fn receipt(db: &Connection, dataset: &str, request: &str) -> Result<Value> {
    ensure!(uuid(request), Rejected::Invalid);
    check_dataset(db, dataset)?;
    let request = request.to_ascii_lowercase();
    Ok(read_delivery(db, &request)?
        .ok_or(Rejected::Missing)?
        .wrap(&request, false))
}

fn current_expected(db: &Connection, target: &MissTarget) -> Result<Expected> {
    let (availability_version, schedule_version, introduced, authoring_revision) = db
        .query_row(
            "SELECT p.availability_version,p.schedule_version,
          EXISTS(SELECT 1 FROM learning WHERE card_id=p.card_id),c.authoring
         FROM prepared_queue p CROSS JOIN daily_clock c
         WHERE p.card_id=?1 AND p.active=1 AND p.kind='opening'",
            [&target.card_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, bool>(2)?, r.get(3)?)),
        )
        .optional()?
        .ok_or(Rejected::TargetChanged)?;
    Ok(Expected {
        availability_version,
        schedule_version,
        authoring_revision,
        reply_revision: catalog::authored_reply_revision(db, &target.repertoire_id)?,
        learning_state: if introduced {
            LearningState::Introduced
        } else {
            LearningState::Unseen
        },
    })
}

fn verified_game(
    db: &Connection,
    source: &Archived<'_>,
    position: &str,
) -> Result<(MissTarget, Value)> {
    let (color, decision) =
        game_feedback::miss_decision(db, &source.pair, source.basis, position, source.plies)?;
    let target = catalog::active_miss_reply(db, &source.pair.repertoire, position)?
        .filter(|t| {
            t.color == color
                && t.expected_uci == decision.expected_uci
                && t.answer_version == decision.answer_version
        })
        .ok_or(Rejected::TargetChanged)?;
    let origin = games::miss_origin(db, &source.pair.game)?;
    Ok((target, json!({"decision":decision,"origin":origin})))
}

pub fn preview(db: &mut Connection, input: Preview) -> Result<Value> {
    ensure!(uuid(&input.request_id), Rejected::Invalid);
    input.source.validate()?;
    input.provenance.validate()?;
    let snapshot = db.transaction()?;
    check_dataset(&snapshot, &input.dataset)?;
    let request_id = input.request_id.to_ascii_lowercase();
    // Preview never binds a request. A known semantic event may be inspected
    // even after its report or card has disappeared.
    let position = input.position_id.as_deref().unwrap_or_default();
    let source = input.source.archived();
    ensure!(
        match source {
            Some(_) => id(position) && input.card_id.is_none(),
            None => input.card_id.as_deref().is_some_and(id) && input.position_id.is_none(),
        },
        Rejected::Invalid
    );
    if let Some(delivery) = read_event(&snapshot, &input.source.event(&input.dataset, position))? {
        return Ok(delivery.wrap(&request_id, false));
    }
    let (target, evidence) = match source {
        Some(source) => verified_game(&snapshot, &source, position)?,
        None => (
            catalog::active_miss_target(&snapshot, input.card_id.as_deref().unwrap())?
                .ok_or(Rejected::TargetChanged)?,
            Value::Null,
        ),
    };
    let expected = current_expected(&snapshot, &target)?;
    let planning = crate::opening_priority::preview_status(
        &snapshot,
        &target.card_id,
        expected.availability_version,
    )?;
    let command = Command {
        contract_version: CONTRACT,
        dataset: input.dataset,
        request_id,
        source: input.source,
        target,
        expected,
        provenance: input.provenance,
    };
    Ok(json!({"delivery":"preview","command":command,"evidence":evidence,"priority":planning}))
}

pub enum Prepared {
    Replay(Value),
    Apply(Box<Apply>),
}

pub struct Apply {
    command: Command,
    fingerprint: String,
    event: String,
    evidence: Value,
    seen_event: bool,
}

impl Command {
    pub fn prepare(mut self, db: &mut Connection) -> Result<Prepared> {
        ensure!(
            self.contract_version == CONTRACT && uuid(&self.request_id),
            Rejected::Invalid
        );
        self.source.validate()?;
        self.provenance.validate()?;
        ensure!(
            id(&self.target.card_id)
                && id(&self.target.repertoire_id)
                && id(&self.target.position_id)
                && id(&self.target.answer_version)
                && matches!(self.target.color.as_str(), "white" | "black")
                && (4..=5).contains(&self.target.expected_uci.len()),
            Rejected::Invalid
        );
        self.request_id.make_ascii_lowercase();
        if let Source::Manual { event_id } = &mut self.source {
            event_id.make_ascii_lowercase();
        }
        let snapshot = db.transaction()?;
        check_dataset(&snapshot, &self.dataset)?;
        let fingerprint = blake3::hash(&serde_json::to_vec(&self)?)
            .to_hex()
            .to_string();
        if let Some(delivery) = read_delivery(&snapshot, &self.request_id)? {
            ensure!(
                delivery.fingerprint.as_deref() == Some(&fingerprint),
                Rejected::Conflict
            );
            return Ok(Prepared::Replay(delivery.wrap(&self.request_id, false)));
        }
        let event = self.source.event(&self.dataset, &self.target.position_id);
        let seen_event = read_event(&snapshot, &event)?.is_some();
        let evidence = if seen_event {
            Value::Null
        } else if let Some(source) = self.source.archived() {
            let (target, evidence) = verified_game(&snapshot, &source, &self.target.position_id)?;
            ensure!(target == self.target, Rejected::TargetChanged);
            evidence
        } else {
            Value::Null
        };
        Ok(Prepared::Apply(Box::new(Apply {
            command: self,
            fingerprint,
            event,
            evidence,
            seen_event,
        })))
    }
}

#[derive(Serialize)]
struct Memory {
    stability: f64,
    difficulty: f64,
    last_at: i64,
    due_at: i64,
    schedule_version: i64,
}

fn memory(db: &Connection, target: &str) -> Result<Memory> {
    Ok(db.query_row(
        "SELECT l.stability,l.difficulty,l.last_at,l.due_at,p.schedule_version
         FROM learning l JOIN prepared_queue p ON p.card_id=l.card_id WHERE l.card_id=?1",
        [target],
        |r| {
            Ok(Memory {
                stability: r.get(0)?,
                difficulty: r.get(1)?,
                last_at: r.get(2)?,
                due_at: r.get(3)?,
                schedule_version: r.get(4)?,
            })
        },
    )?)
}

impl Apply {
    pub fn accept(self, db: &mut Connection) -> Result<Value> {
        self.accept_with_steps(db, VM_LIMIT - 1_000)
    }

    fn accept_with_steps(self, db: &mut Connection, steps: u64) -> Result<Value> {
        let before = db.total_changes();
        let tx = db.transaction_with_behavior(Immediate)?;
        let mut remaining = steps;
        tx.progress_handler(
            1,
            Some(move || {
                remaining = remaining.saturating_sub(1);
                remaining == 0
            }),
        );
        let result = self.accept_bounded(&tx, before);
        // An interruption can invalidate the whole transaction. Remove the
        // handler before the outer transaction commits or rolls back.
        tx.progress_handler(0, None::<fn() -> bool>);
        let value = result?;
        tx.commit()?;
        Ok(value)
    }

    fn accept_bounded(self, tx: &Connection, before: u64) -> Result<Value> {
        let command = &self.command;
        check_dataset(tx, &command.dataset)?;
        let request = &command.request_id;
        if let Some(delivery) = read_delivery(tx, request)? {
            ensure!(
                delivery.fingerprint.as_deref() == Some(&self.fingerprint),
                Rejected::Conflict
            );
            return Ok(delivery.wrap(request, false));
        }
        if let Some(delivery) = read_event(tx, &self.event)? {
            tx.execute(
                "INSERT INTO opening_miss_requests VALUES (?1,?2,?3)",
                params![request, self.fingerprint, delivery.original],
            )?;
            return Ok(delivery.wrap(request, false));
        }
        // A delivery observed in the read snapshot must survive in this dataset.
        ensure!(!self.seen_event, Rejected::Dataset);
        let target = &command.target;
        ensure!(
            catalog::active_miss_target(tx, &target.card_id)?.as_ref() == Some(target),
            Rejected::TargetChanged
        );
        let actual = current_expected(tx, target)?;
        let expected = &command.expected;
        ensure!(
            actual.availability_version == expected.availability_version,
            Rejected::TargetChanged
        );
        ensure!(
            actual.reply_revision == expected.reply_revision
                && actual.authoring_revision == expected.authoring_revision,
            Rejected::AuthoringChanged
        );
        if let Some(source) = command.source.archived() {
            ensure!(
                game_feedback::check_miss_basis(tx, &source.pair, source.basis)? == target.color,
                Rejected::TargetChanged
            );
        }
        ensure!(
            actual.learning_state == expected.learning_state,
            Rejected::EligibilityChanged
        );
        ensure!(
            actual.schedule_version == expected.schedule_version,
            Rejected::LearningChanged
        );
        let introduced = actual.learning_state == LearningState::Introduced;
        if introduced {
            ensure!(
                study_store::unanswered_card(tx)?.as_deref() != Some(&target.card_id),
                Rejected::ActiveStudy
            );
        }
        let now = Utc::now().timestamp();
        let (assessment, rating, repeat) = if introduced {
            let prior = memory(tx, &target.card_id)?;
            let assessment = tx.query_row(
                "INSERT INTO attempts(card_id,result)
                 VALUES (?1,'{\"surface\":\"external_opening_miss_v1\"}') RETURNING id",
                [&target.card_id],
                |r| r.get::<_, i64>(0),
            )?;
            let (_, previous) = study_store::state(tx)?;
            // Keep 12 trigger-inclusive writes for deferral, goal, generation and receipt.
            let repeat = review::external_again(
                tx,
                &target.card_id,
                assessment,
                previous.as_deref(),
                now,
                before + WRITE_LIMIT - 12,
            )?;
            let after = memory(tx, &target.card_id)?;
            (
                Some(assessment),
                json!({"before":prior,"after":after}),
                serde_json::to_value(repeat)?,
            )
        } else {
            (None, Value::Null, Value::Null)
        };
        let (goal, created) =
            crate::opening_priority::join(tx, &target.card_id, actual.availability_version)?;
        let receipt = json!({
            "request_id":request,"event":serde_json::from_str::<Value>(&self.event)?,
            "dataset":command.dataset,"contract_version":CONTRACT,"accepted_at":now,
            "source":command.source,"target":target,"expected":expected,
            "provenance":command.provenance,"evidence":self.evidence,
            "learning_effect":if introduced { "again" } else { "none_unseen" },
            "assessment_id":assessment,"rating":rating,"repeat":repeat,
            "priority":{"goal_id":goal,"intent":if created { "recorded" } else { "joined_existing" }},
        });
        let body = serde_json::to_string(&receipt)?;
        ensure!(
            body.len() <= 32 * 1024,
            "opening miss receipt exceeds its bound"
        );
        tx.execute(
            "INSERT INTO opening_miss_receipts VALUES (?1,?2,?3,?4,?5)",
            params![request, self.event, assessment, goal, body],
        )?;
        tx.execute(
            "INSERT INTO opening_miss_requests VALUES (?1,?2,?1)",
            params![request, self.fingerprint],
        )?;
        ensure!(
            tx.total_changes() - before <= WRITE_LIMIT,
            "opening miss exceeds its write bound"
        );
        Ok(Delivery {
            original: request.clone(),
            receipt,
            fingerprint: None,
        }
        .wrap(request, true))
    }
}

pub fn status(db: &Connection, dataset: &str, goal: i64) -> Result<Value> {
    check_dataset(db, dataset)?;
    crate::opening_priority::status(db, goal)?.ok_or_else(|| Rejected::Missing.into())
}

pub fn cancel(db: &mut Connection, dataset: &str, goal: i64) -> Result<Value> {
    let tx = db.transaction_with_behavior(Immediate)?;
    check_dataset(&tx, dataset)?;
    crate::opening_priority::cancel(&tx, goal)?;
    let reply = crate::opening_priority::status(&tx, goal)?.ok_or(Rejected::Missing)?;
    tx.commit()?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chess,
        db::Database,
        learning::{self, Rating},
        queue_store, study,
    };
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    fn fixture() -> Result<(tempfile::TempDir, Connection, String)> {
        let dir = tempfile::tempdir()?;
        let mut db = Database::init(&dir.path().join("miss.sqlite3"))?;
        catalog::save_opening(
            &db,
            &catalog::OpeningInput {
                name: "Queen's gambit".into(),
                color: "white".into(),
                start_at: 0,
                route: chess::Route {
                    fen: chess::fen(&chess::position("")?),
                    moves: ["d2d4", "d7d5", "c2c4"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                },
            },
            None,
        )?;
        while !queue_store::seed_batch(&mut db)? {}
        let card = db.query_row(
            "SELECT id FROM active_cards WHERE answers='[\"c2c4\"]'",
            [],
            |r| r.get(0),
        )?;
        Ok((dir, db, card))
    }

    fn manual(db: &mut Connection, card: &str, request: usize, event: usize) -> Result<Value> {
        let input = serde_json::from_value(json!({
            "dataset":db::dataset(db)?,"card_id":card,
            "request_id":format!("12345678-1234-1234-1234-{request:012}"),
            "source":{"kind":"manual","event_id":format!("87654321-1234-1234-1234-{event:012}")},
        }))?;
        Ok(preview(db, input)?["command"].clone())
    }

    fn submit(db: &mut Connection, payload: &Value) -> Result<Value> {
        let command: Command = serde_json::from_value(payload.clone())?;
        match command.prepare(db)? {
            Prepared::Replay(v) => Ok(v),
            Prepared::Apply(apply) => apply.accept(db),
        }
    }

    fn count(db: &Connection, table: &str) -> Result<usize> {
        Ok(db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
    }

    #[test]
    fn exact_unseen_replay_and_semantic_duplicate_survive_real_introduction_and_source_denial()
    -> Result<()> {
        let (_dir, mut db, card) = fixture()?;
        db.authorizer(Some(|ctx: AuthContext| {
            if matches!(
                ctx.action,
                AuthAction::Read {
                    table_name: "games" | "jobs" | "repertoire_feedback" | "mismatches",
                    ..
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }));
        let first = manual(&mut db, &card, 1, 1)?;
        let second = manual(&mut db, &card, 2, 1)?;
        let receipt = submit(&mut db, &first)?["receipt"].clone();
        assert_eq!(receipt["learning_effect"], "none_unseen");
        assert_eq!(count(&db, "learning")?, 0);
        assert_eq!(count(&db, "attempts")?, 0);
        let goal = receipt["priority"]["goal_id"].as_i64().unwrap();
        let dataset = db::dataset(&db)?;
        assert_eq!(status(&db, &dataset, goal)?["state"], "preparing");
        cancel(&mut db, &dataset, goal)?;
        let now = Utc::now().timestamp();
        review::record_rating(&db, &card, 99, None, Rating::Good, now, None)?;
        let before = db.total_changes();
        let replay = submit(&mut db, &first)?;
        assert_eq!(replay["delivery"], "replayed");
        assert_eq!(replay["receipt"], receipt);
        assert_eq!(db.total_changes(), before);
        let duplicate = submit(&mut db, &second)?;
        assert_eq!(duplicate["delivery"], "already_processed");
        assert_eq!(duplicate["receipt"], receipt);
        let mut changed = second.clone();
        changed["provenance"]["note"] = json!("changed");
        assert!(matches!(
            submit(&mut db, &changed).unwrap_err().downcast_ref(),
            Some(Rejected::Conflict)
        ));
        assert_eq!(status(&db, &dataset, goal)?["state"], "cancelled");
        assert_eq!(count(&db, "opening_miss_receipts")?, 1);
        assert_eq!(count(&db, "attempts")?, 0);
        Ok(())
    }

    #[test]
    fn distinct_again_events_share_pending_opportunity_and_preserve_study() -> Result<()> {
        let (_dir, mut db, card) = fixture()?;
        let now = Utc::now().timestamp();
        review::record_rating(&db, &card, 100, None, Rating::Good, now, None)?;
        let prior = db::prior_review(&db, &card)?;
        let first = manual(&mut db, &card, 1, 1)?;
        let second = manual(&mut db, &card, 2, 2)?;
        let other: i64 = db.query_row(
            "INSERT INTO attempts(card_id) SELECT id FROM cards WHERE id<>?1 RETURNING id",
            [&card],
            |r| r.get(0),
        )?;
        db.execute(
            "UPDATE study SET active=?1,last_kind='opening' WHERE singleton=1",
            [other],
        )?;
        let active = study_store::state(&db)?;
        let r1 = submit(&mut db, &first)?["receipt"].clone();
        let accepted = r1["accepted_at"].as_i64().unwrap();
        let expected = learning::schedule(Rating::Again, prior, accepted)?;
        assert!(
            (r1["rating"]["after"]["stability"].as_f64().unwrap() - f64::from(expected.stability))
                .abs()
                < 1e-6
        );
        assert_eq!(r1["rating"]["after"]["due_at"], json!(expected.due_at));
        assert_eq!(r1["repeat"]["kind"], "deferred");
        let obligation = r1["repeat"]["obligation"].clone();
        assert!(matches!(
            submit(&mut db, &second).unwrap_err().downcast_ref(),
            Some(Rejected::LearningChanged)
        ));
        let mut finalized = second.clone();
        finalized["expected"]["schedule_version"] =
            r1["rating"]["after"]["schedule_version"].clone();
        let r2 = submit(&mut db, &finalized)?["receipt"].clone();
        assert_eq!(r2["repeat"]["kind"], "already_pending");
        assert_eq!(r2["repeat"]["obligation"], obligation);
        assert_eq!(r2["priority"]["goal_id"], r1["priority"]["goal_id"]);
        assert_eq!(count(&db, "repeat_obligation")?, 1);
        assert_eq!(count(&db, "opening_miss_receipts")?, 2);
        assert_eq!(study_store::state(&db)?, active);
        let forged = study::AnswerIntentV1 {
            attempt: r1["assessment_id"].as_i64().unwrap(),
            dataset: db::dataset(&db)?,
            action: study::AnswerAction::Play("c2c4".into()),
        };
        assert!(matches!(
            review::answer(&mut db, forged)
                .err()
                .unwrap()
                .downcast_ref(),
            Some(study::Conflict::ExternalAssessment)
        ));
        Ok(())
    }

    #[test]
    fn preview_branch_and_active_attempt_fences_are_atomic() -> Result<()> {
        let (_dir, mut db, card) = fixture()?;
        let unseen = manual(&mut db, &card, 1, 1)?;
        let now = Utc::now().timestamp();
        review::record_rating(&db, &card, 100, None, Rating::Good, now, None)?;
        assert!(matches!(
            submit(&mut db, &unseen).unwrap_err().downcast_ref(),
            Some(Rejected::EligibilityChanged)
        ));
        let introduced = manual(&mut db, &card, 2, 1)?;
        let active = db.query_row(
            "INSERT INTO attempts(card_id) VALUES (?1) RETURNING id",
            [&card],
            |r| r.get::<_, i64>(0),
        )?;
        db.execute("UPDATE study SET active=?1", [active])?;
        let before = db.total_changes();
        assert!(matches!(
            submit(&mut db, &introduced).unwrap_err().downcast_ref(),
            Some(Rejected::ActiveStudy)
        ));
        assert_eq!(db.total_changes(), before);
        assert_eq!(count(&db, "opening_priority")?, 0);
        assert_eq!(count(&db, "opening_miss_receipts")?, 0);
        Ok(())
    }

    #[test]
    fn vm_interrupt_rolls_back_and_the_original_request_can_retry() -> Result<()> {
        let (_dir, mut db, card) = fixture()?;
        let now = Utc::now().timestamp();
        review::record_rating(&db, &card, 100, None, Rating::Good, now, None)?;
        let prior = serde_json::to_value(memory(&db, &card)?)?;
        let command = manual(&mut db, &card, 1, 1)?;
        let Prepared::Apply(prepared) =
            serde_json::from_value::<Command>(command.clone())?.prepare(&mut db)?
        else {
            unreachable!("new request")
        };
        let error = prepared.accept_with_steps(&mut db, 60).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<rusqlite::Error>()
                .and_then(rusqlite::Error::sqlite_error_code),
            Some(rusqlite::ErrorCode::OperationInterrupted)
        );
        assert!(db.is_autocommit());
        assert_eq!(serde_json::to_value(memory(&db, &card)?)?, prior);
        assert_eq!(count(&db, "opening_miss_receipts")?, 0);
        assert_eq!(submit(&mut db, &command)?["delivery"], "accepted");
        Ok(())
    }

    #[test]
    fn failure_after_again_rolls_back_the_assessment_repeat_and_goal() -> Result<()> {
        let (_dir, mut db, card) = fixture()?;
        let now = Utc::now().timestamp();
        review::record_rating(&db, &card, 100, None, Rating::Good, now, None)?;
        let prior = serde_json::to_value(memory(&db, &card)?)?;
        let command = manual(&mut db, &card, 1, 1)?;
        db.execute_batch(
            "CREATE TEMP TRIGGER reject_miss BEFORE INSERT ON opening_miss_receipts
            BEGIN SELECT raise(ABORT,'injected receipt write failure'); END;",
        )?;
        assert!(submit(&mut db, &command).is_err());
        assert_eq!(serde_json::to_value(memory(&db, &card)?)?, prior);
        for table in [
            "attempts",
            "repeat_obligation",
            "deck",
            "opening_priority",
            "opening_miss_receipts",
        ] {
            assert_eq!(count(&db, table)?, 0, "{table}");
        }
        db.execute_batch("DROP TRIGGER reject_miss")?;
        assert_eq!(submit(&mut db, &command)?["delivery"], "accepted");
        Ok(())
    }
}
