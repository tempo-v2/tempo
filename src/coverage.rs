use crate::{
    ApiError, ApiResult, App, Database, ViewResult, catalog, chess, db, optional_read,
    optional_view, write_db,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    extract::{Path, State},
};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};
use tokio::sync::Semaphore;

#[derive(Serialize, Deserialize, Clone)]
pub struct Scan {
    pub repertoire: String,
    pub color: String,
    route: chess::Route,
    through: u32,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Gap {
    pub fen: String,
    moves: Vec<String>,
    probability: f64,
}

#[derive(Serialize, Deserialize)]
pub struct Forecast {
    #[serde(default)]
    model: ForecastModel,
    gaps: Vec<Gap>,
    pub reach: HashMap<String, f64>,
    paths: Vec<(Vec<String>, f64)>,
    ended: f64,
    conflict: f64,
    unresolved: f64,
}

const CURRENT_MODEL: &str = "maia3-5m-1500-history-v1";

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
enum ForecastModel {
    #[default]
    Maia2,
    #[serde(rename = "maia3-5m-1500-history-v1")]
    Maia3,
}

impl ForecastModel {
    fn is_current(self) -> bool {
        self == Self::Maia3
    }

    fn label(self) -> &'static str {
        match self {
            Self::Maia2 => "Maia-2 Rapid 1500/1500",
            Self::Maia3 => "Maia-3 5M 1500/1500",
        }
    }
}

#[derive(Debug)]
pub struct SupersededModel;
impl std::fmt::Display for SupersededModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "This report used an older Maia model. Rescan before applying the opening order.",
        )
    }
}
impl std::error::Error for SupersededModel {}

const PAGE_SIZE: usize = 30;
const MAX_NODES: usize = 3000;
const MAX_PATH: usize = 100;

struct Projection {
    model: ForecastModel,
    header: String,
    studied: String,
    pages: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct StudyInput {
    cards: Vec<String>,
    paths: Vec<(Vec<usize>, f64)>,
}

impl Projection {
    fn prepare(forecast: &Forecast) -> Result<Self> {
        ensure!(
            forecast.gaps.len() <= MAX_NODES && forecast.paths.len() <= MAX_NODES,
            "Coverage forecast exceeds its node budget"
        );
        let mut ids = HashMap::new();
        let mut cards = Vec::new();
        let mut paths = Vec::new();
        let mut authored = 0.0;
        for (path, mass) in &forecast.paths {
            ensure!(
                path.len() <= MAX_PATH,
                "Coverage path exceeds its move budget"
            );
            authored += mass;
            let path = path
                .iter()
                .map(|card| {
                    *ids.entry(card).or_insert_with(|| {
                        let index = cards.len();
                        cards.push(card.to_owned());
                        index
                    })
                })
                .collect();
            paths.push((path, *mass));
        }
        ensure!(
            cards.len() <= MAX_NODES,
            "Coverage forecast exceeds its card budget"
        );
        let studied = serde_json::to_string(&StudyInput { cards, paths })?;
        ensure!(
            studied.len() <= 2 * 1024 * 1024,
            "Coverage summary exceeds its byte budget"
        );
        let header = serde_json::to_string(&json!({
            "authored": authored, "ended": forecast.ended, "conflict": forecast.conflict,
            "unresolved": forecast.unresolved, "total": forecast.gaps.len()
        }))?;
        let pages = forecast
            .gaps
            .chunks(PAGE_SIZE)
            .map(|page| {
                let encoded = serde_json::to_string(page)?;
                ensure!(
                    encoded.len() <= 256 * 1024,
                    "Coverage page exceeds its byte budget"
                );
                Ok(encoded)
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            model: forecast.model,
            header,
            studied,
            pages,
        })
    }

    fn insert(&self, db: &Connection, id: &str) -> Result<()> {
        db.execute(
            "INSERT INTO coverage_view VALUES(?1,?2,?3,?4)",
            (
                id,
                &self.header,
                &self.studied,
                serde_json::to_string(&self.model)?,
            ),
        )?;
        let mut insert = db.prepare_cached("INSERT INTO coverage_gap_page VALUES(?1,?2,?3)")?;
        for (page, gaps) in self.pages.iter().enumerate() {
            insert.execute((id, page, gaps))?;
        }
        Ok(())
    }
}

pub fn backfill_views(db: &Connection) -> Result<()> {
    let mut query = db.prepare("SELECT id,report FROM coverage_report")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let forecast = serde_json::from_str(&row.get::<_, String>(1)?)?;
        Projection::prepare(&forecast)?.insert(db, &id)?;
    }
    Ok(())
}

pub use catalog::authored_reply_revision as revision;

pub fn load(c: &Connection, id: &str) -> Result<(Scan, i64, Forecast)> {
    let (request, rev, report): (String, i64, String) = c
        .query_row(
            "SELECT request,revision,report FROM coverage_report WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .context("Run the coverage scan again.")?;
    Ok((
        serde_json::from_str(&request)?,
        rev,
        serde_json::from_str(&report)?,
    ))
}

fn snapshot(path: &std::path::Path, scan: &Scan) -> Result<(String, i64, Value)> {
    let mut c = Database::read(path)?;
    let tx = c.transaction()?;
    catalog::existing_repertoire(&tx, &scan.repertoire, &scan.color)?;
    let rev = revision(&tx, &scan.repertoire)?;
    let id = chess::id(&[
        CURRENT_MODEL,
        &db::dataset(&tx)?,
        &rev.to_string(),
        &serde_json::to_string(scan)?,
    ]);
    let mut query=tx.prepare("SELECT id,fen,answers,quarantined FROM cards WHERE kind='opening' AND opening_retired=0 AND repertoire_id=?1 AND color=?2")?;
    let mut rows = query.query((&scan.repertoire, &scan.color))?;
    let mut cards = vec![];
    while let Some(r) = rows.next()? {
        cards.push(json!({"id":r.get::<_,String>(0)?,"fen":r.get::<_,String>(1)?,"answers":serde_json::from_str::<Vec<String>>(&r.get::<_,String>(2)?)?,"quarantined":r.get::<_,bool>(3)?}));
    }
    let mut input = serde_json::to_value(scan)?;
    input["cards"] = json!(cards);
    Ok((id, rev, input))
}

fn evaluate(input: Value) -> Result<Forecast> {
    let root = PathBuf::from(std::env::var_os("HOME").context("Missing home directory")?)
        .join("Library/Application Support/Tempo Lean/Maia3");
    let mut child = Command::new(root.join("venv/bin/python"))
        .arg("-c")
        .arg(include_str!("../tools/coverage.py"))
        .arg(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("Maia is unavailable. Run tools/setup-coverage.sh on the Tempo host.")?;
    let mut stdin = child.stdin.take().unwrap();
    let written = stdin.write_all(&serde_json::to_vec(&input)?);
    drop(stdin);
    let output = child.wait_with_output()?;
    written?;
    ensure!(
        output.status.success(),
        "Maia could not finish this scan. Check the host model setup and retry."
    );
    let forecast: Forecast = serde_json::from_slice(&output.stdout)?;
    ensure!(forecast.model.is_current(), "Unexpected forecast model");
    Ok(forecast)
}

static SLOT: Semaphore = Semaphore::const_new(1);

pub async fn start(State(app): State<App>, Json(mut scan): Json<Scan>) -> ApiResult {
    let permit = SLOT
        .try_acquire()
        .map_err(|_| ApiError(anyhow::anyhow!("A coverage scan is already running.")))?;
    let path = app.database.clone();
    let (id, rev, report, scan) = tokio::task::spawn_blocking(move || -> Result<_> {
        let _permit = permit;
        ensure!(
            scan.through > 0 && scan.through <= 100 && scan.route.moves.len() <= 200,
            "Choose a horizon from 1 to 100 and a starting line of at most 200 plies."
        );
        scan.route.fen = chess::fen(&chess::position(&scan.route.fen)?);
        chess::board_at(&scan.route, scan.route.moves.len())?;
        let (id, rev, input) = snapshot(&path, &scan)?;
        let cached = Database::read(&path)?.query_row(
            "SELECT EXISTS(SELECT 1 FROM coverage_report WHERE id=?1)",
            [&id],
            |r| r.get::<_, bool>(0),
        )?;
        let report = if cached {
            None
        } else {
            let forecast = evaluate(input)?;
            Some((
                serde_json::to_string(&forecast)?,
                Projection::prepare(&forecast)?,
            ))
        };
        Ok((id, rev, report, scan))
    })
    .await
    .map_err(|e| ApiError(e.into()))?
    .map_err(ApiError)?;
    write_db(app, move |c| {
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(
            revision(&tx, &scan.repertoire)? == rev,
            "The repertoire changed. Retry scan."
        );
        if let Some((report, projection)) = &report {
            let before = tx.total_changes();
            if tx.execute(
                "INSERT OR IGNORE INTO coverage_report VALUES(?1,?2,?3,?4,?5)",
                (
                    &id,
                    &scan.repertoire,
                    rev,
                    serde_json::to_string(&scan)?,
                    report,
                ),
            )? != 0
            {
                projection.insert(&tx, &id)?;
            }
            ensure!(
                tx.total_changes() - before <= 128,
                "Coverage publication exceeds row budget"
            );
        }
        tx.commit()?;
        Ok(json!({"id":id}))
    })
    .await
}

#[derive(Default, Deserialize)]
pub struct Page {
    #[serde(default)]
    offset: usize,
}
fn scan_metadata(db: &Connection, id: &str) -> Result<(Scan, i64)> {
    let (request, rev): (String, i64) = db
        .query_row(
            "SELECT request,revision FROM coverage_report WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .context("Run the coverage scan again.")?;
    Ok((serde_json::from_str(&request)?, rev))
}

fn gap_chunk(db: &Connection, id: &str, page: usize) -> Result<Vec<Gap>> {
    let encoded: Option<String> = db
        .query_row(
            "SELECT gaps FROM coverage_gap_page WHERE report_id=?1 AND page=?2",
            (id, page),
            |r| r.get(0),
        )
        .optional()?;
    Ok(encoded
        .map(|json| serde_json::from_str(&json))
        .transpose()?
        .unwrap_or_default())
}

fn page_view(db: &Connection, id: &str, offset: usize) -> Result<Value> {
    let (scan, rev) = scan_metadata(db, id)?;
    let (encoded, model): (String, String) = db.query_row(
        "SELECT header,model_id FROM coverage_view WHERE report_id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let model: ForecastModel = serde_json::from_str(&model)?;
    let mut header: Value = serde_json::from_str(&encoded)?;
    header["model"] = json!(model.label());
    header["model_superseded"] = json!(!model.is_current());
    let total = header["total"]
        .as_u64()
        .context("Missing coverage gap count")? as usize;
    let mut gaps = Vec::new();
    if offset < total {
        gaps.extend(
            gap_chunk(db, id, offset / PAGE_SIZE)?
                .into_iter()
                .skip(offset % PAGE_SIZE),
        );
        if gaps.len() < PAGE_SIZE && offset + gaps.len() < total {
            gaps.extend(
                gap_chunk(db, id, offset / PAGE_SIZE + 1)?
                    .into_iter()
                    .take(PAGE_SIZE - gaps.len()),
            );
        }
    }
    header["stale"] = json!(revision(db, &scan.repertoire)? != rev);
    header["id"] = json!(id);
    header["scan"] = serde_json::to_value(scan)?;
    header["gaps"] = serde_json::to_value(gaps)?;
    Ok(header)
}

fn studied_coverage(db: &Connection, id: &str) -> Result<f64> {
    let encoded: String = db.query_row(
        "SELECT studied_input FROM coverage_view WHERE report_id=?1",
        [id],
        |r| r.get(0),
    )?;
    let StudyInput { cards, paths } = serde_json::from_str(&encoded)?;
    let mut seen = vec![false; cards.len()];
    let mut query = db.prepare_cached(
        "SELECT j.key FROM json_each(?1) j CROSS JOIN learning l WHERE l.card_id=j.value",
    )?;
    let mut rows = query.query([serde_json::to_string(&cards)?])?;
    while let Some(row) = rows.next()? {
        seen[row.get::<_, usize>(0)?] = true;
    }
    Ok(paths
        .into_iter()
        .filter(|(path, _)| path.iter().all(|&card| seen[card]))
        .map(|(_, mass)| mass)
        .sum())
}

pub async fn read(
    State(app): State<App>,
    Path(id): Path<String>,
    axum::extract::Query(page): axum::extract::Query<Page>,
) -> ViewResult {
    optional_view(app, optional_read::Reader::Coverage, move |c| {
        let tx = c.transaction()?;
        let mut view = page_view(&tx, &id, page.offset)?;
        view["studied"] = json!(studied_coverage(&tx, &id)?);
        Ok(view)
    })
    .await
}

pub async fn gaps(
    State(app): State<App>,
    Path(id): Path<String>,
    axum::extract::Query(page): axum::extract::Query<Page>,
) -> ViewResult {
    optional_view(app, optional_read::Reader::CoverageGaps, move |c| {
        let tx = c.transaction()?;
        page_view(&tx, &id, page.offset)
    })
    .await
}

pub async fn studied(State(app): State<App>, Path(id): Path<String>) -> ViewResult {
    optional_view(app, optional_read::Reader::CoverageStudied, move |c| {
        let tx = c.transaction()?;
        Ok(json!({"id":id,"studied":studied_coverage(&tx, &id)?}))
    })
    .await
}

pub fn selected(c: &Connection, id: &str, index: usize) -> Result<(Scan, i64, Gap)> {
    let (scan, rev) = scan_metadata(c, id)?;
    let gap = gap_chunk(c, id, index / PAGE_SIZE)?
        .into_iter()
        .nth(index % PAGE_SIZE)
        .context("Unknown coverage gap")?;
    Ok((scan, rev, gap))
}

#[derive(Deserialize)]
pub struct Apply {
    command: String,
}

pub async fn apply(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(input): Json<Apply>,
) -> ApiResult {
    static APPLY: Semaphore = Semaphore::const_new(1);
    let permit = APPLY
        .try_acquire()
        .map_err(|_| ApiError(crate::writer::Unavailable.into()))?;
    let reply = tokio::task::spawn_blocking(move || -> Result<Value> {
        let _permit = permit;
        ensure!(
            input.command.len() == 36 && input.command.is_ascii(),
            "Invalid apply command"
        );
        let mut db = Database::read(&app.database)?;
        let tx = db.transaction()?;
        let (scan, rev, forecast) = load(&tx, &id)?;
        let reply = json!({"id":id,"command":input.command,"cards":forecast.reach.len()});
        let prior: Option<String> = tx
            .query_row(
                "SELECT report FROM applied_forecast WHERE command=?1",
                [&input.command],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(prior) = prior {
            ensure!(prior == id, "Apply identity already used");
            return Ok(reply);
        }
        ensure!(forecast.model.is_current(), SupersededModel);
        ensure!(
            revision(&tx, &scan.repertoire)? == rev,
            crate::gap_edit::Stale
        );
        let order = crate::opening_rank::StagedOrder::prepare(&tx, &id, &forecast.reach)?;
        tx.commit()?;
        let bank = order.stage(&app.writer)?;
        app.writer.background(move |c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            ensure!(
                revision(&tx, &scan.repertoire)? == rev,
                crate::gap_edit::Stale
            );
            tx.execute(
                "UPDATE forecast_order_state SET bank=?1 WHERE singleton=1",
                [bank],
            )?;
            tx.execute(
                "INSERT INTO applied_forecast VALUES(?1,?2)",
                (&input.command, &id),
            )?;
            tx.commit()?;
            Ok(())
        })?;
        Ok(reply)
    })
    .await
    .map_err(|e| ApiError(e.into()))?
    .map_err(ApiError)?;
    Ok(Json(reply))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, response::IntoResponse};

    #[test]
    fn coverage_pages_do_not_read_learning_or_forecast_and_summary_is_current() -> Result<()> {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        let directory = tempfile::tempdir()?;
        let db = Database::init(&directory.path().join("views.sqlite"))?;
        let scan = Scan {
            repertoire: "fixture".into(),
            color: "white".into(),
            through: 10,
            route: chess::Route {
                fen: "fixture".into(),
                moves: vec![],
            },
        };
        let forecast = Forecast {
            model: ForecastModel::Maia3,
            gaps: (0..65)
                .map(|index| Gap {
                    fen: index.to_string(),
                    moves: vec![],
                    probability: 0.01,
                })
                .collect(),
            reach: HashMap::new(),
            paths: vec![
                (vec!["first".into()], 0.25),
                (vec!["first".into(), "second".into()], 0.5),
            ],
            ended: 0.0,
            conflict: 0.0,
            unresolved: 0.25,
        };
        db.execute(
            "INSERT INTO coverage_report VALUES('fixture','fixture',0,?1,?2)",
            (
                serde_json::to_string(&scan)?,
                serde_json::to_string(&forecast)?,
            ),
        )?;
        backfill_views(&db)?;
        db.authorizer(Some(|context: AuthContext<'_>| match context.action {
            AuthAction::Read {
                table_name: "learning",
                ..
            }
            | AuthAction::Read {
                table_name: "coverage_report",
                column_name: "report",
            }
            | AuthAction::Read {
                table_name: "coverage_view",
                column_name: "studied_input",
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }));
        for (offset, length) in [(0, 30), (29, 30), (60, 5), (65, 0)] {
            let page = page_view(&db, "fixture", offset)?;
            let gaps = page["gaps"].as_array().unwrap();
            assert_eq!(gaps.len(), length);
            for (index, gap) in gaps.iter().enumerate() {
                assert_eq!(gap["fen"], (offset + index).to_string());
            }
        }
        assert_eq!(selected(&db, "fixture", 64)?.2.fen, "64");
        db.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        assert_eq!(studied_coverage(&db, "fixture")?, 0.0);
        for card in ["first", "second", "unrelated"] {
            db.execute(
                "INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
                VALUES(?1,'tactic','fixture','white','fixture',?1,'[]')",
                [card],
            )?;
            db.execute("INSERT INTO learning VALUES(?1,1,1,1,10)", [card])?;
            assert_eq!(
                studied_coverage(&db, "fixture")?,
                if card == "first" { 0.25 } else { 0.75 }
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn old_report_preserves_commands_but_cannot_set_a_new_order() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("tempo.sqlite3");
        let mut c = Database::init(&database)?;
        let start = chess::position("")?;
        let scan = Scan {
            repertoire: catalog::save_opening(
                &c,
                &catalog::OpeningInput {
                    name: "Test opening".into(),
                    color: "white".into(),
                    route: chess::Route {
                        fen: chess::fen(&start),
                        moves: vec!["e2e4".into()],
                    },
                    start_at: 0,
                },
                None,
            )?,
            color: "white".into(),
            route: chess::Route {
                fen: chess::fen(&start),
                moves: vec![],
            },
            through: 2,
        };
        crate::queue_store::seed_batch(&mut c)?;
        let revision = revision(&c, &scan.repertoire)?;
        let mut gap = start.clone();
        for movement in ["e2e4", "e7e5"] {
            use pgn_reader::shakmaty::Position;
            let played = chess::parse_move(&gap, movement)?;
            gap.play_unchecked(played);
        }
        let card: String = c.query_row("SELECT id FROM cards WHERE kind='opening'", [], |row| {
            row.get(0)
        })?;
        let mut report = json!({
            "gaps": [{"fen":chess::fen(&gap), "moves":["e2e4","e7e5"],"probability":1.0}],
            "reach": {card: 1.0}, "paths":[], "ended":0., "conflict":0., "unresolved":0.
        });
        for id in ["old", "new"] {
            if id == "new" {
                report["model"] = json!(CURRENT_MODEL);
            }
            c.execute(
                "INSERT INTO coverage_report VALUES (?1,?2,?3,?4,?5)",
                (
                    id,
                    &scan.repertoire,
                    revision,
                    serde_json::to_string(&scan)?,
                    report.to_string(),
                ),
            )?;
        }

        backfill_views(&c)?;
        let accepted = "11111111-1111-4111-8111-111111111111";
        let next = "22222222-2222-4222-8222-222222222222";
        c.execute("INSERT INTO applied_forecast VALUES (?1,'old')", [accepted])?;
        let app = std::sync::Arc::new(crate::Runtime::new(
            database.clone(),
            crate::writer::Writer::start(c, None)?,
            crate::uploads::Staging::start(&database)?,
        ));
        let order = |id: &str, command: &str| {
            apply(
                State(app.clone()),
                Path(id.into()),
                Json(Apply {
                    command: command.into(),
                }),
            )
        };
        for (id, superseded) in [("old", true), ("new", false)] {
            let response = read(
                State(app.clone()),
                Path(id.into()),
                axum::extract::Query(Page::default()),
            )
            .await
            .map_err(|error| error.0)?;
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 65_536).await?)?;
            assert_eq!(body["model_superseded"], superseded);
            assert_eq!(body["stale"], false);
        }
        let replay = order("old", accepted).await.map_err(|error| error.0)?;
        assert_eq!(replay["command"], accepted);
        let rejected = order("old", next)
            .await
            .expect_err("old report accepted")
            .into_response();
        assert_eq!(rejected.status(), axum::http::StatusCode::CONFLICT);
        let body: Value = serde_json::from_slice(&to_bytes(rejected.into_body(), 65_536).await?)?;
        assert_eq!(body["code"], "coverage_model_superseded");
        let applied = order("new", next).await.map_err(|error| error.0)?;
        assert_eq!(applied["cards"], 1);

        let draft = json!({"report":"old","gap":0,"pgn":"1. e4 e5 2. Nf3"});
        let preview = crate::gap_edit::preview(
            State(app.clone()),
            Json(serde_json::from_value(draft.clone())?),
        )
        .await
        .map_err(|error| error.0)?;
        let save = json!({"draft":draft,"command":preview["command"]});
        let submit = || {
            crate::gap_edit::save(
                State(app.clone()),
                Json(serde_json::from_value(save.clone()).unwrap()),
            )
        };
        let receipt = submit().await.map_err(|error| error.0)?;
        assert_eq!(submit().await.map_err(|error| error.0)?.0, receipt.0);
        let c = Database::read(&database)?;
        let receipts: i64 = c.query_row("SELECT count(*) FROM gap_save_receipt", [], |row| {
            row.get(0)
        })?;
        assert_eq!(receipts, 1);
        Ok(())
    }
}
