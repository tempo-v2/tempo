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
    collections::{HashMap, HashSet},
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

#[derive(Serialize, Deserialize)]
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

pub fn revision(c: &Connection, rep: &str) -> Result<i64> {
    Ok(c.query_row(
        "SELECT COALESCE((SELECT revision FROM coverage_revision WHERE repertoire=?1),0)",
        [rep],
        |r| r.get(0),
    )?)
}

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
    let mut query=tx.prepare("SELECT id,fen,answers,quarantined FROM cards WHERE kind='opening' AND repertoire_id=?1 AND color=?2")?;
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
    ensure!(
        forecast.model == ForecastModel::Maia3,
        "Unexpected forecast model"
    );
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
            Some(serde_json::to_string(&evaluate(input)?)?)
        };
        Ok((id, rev, report, scan))
    })
    .await
    .map_err(|e| ApiError(e.into()))?
    .map_err(ApiError)?;
    write_db(app, move |c| {
        ensure!(
            revision(c, &scan.repertoire)? == rev,
            "The repertoire changed. Retry scan."
        );
        if let Some(report) = report {
            c.execute(
                "INSERT OR IGNORE INTO coverage_report VALUES(?1,?2,?3,?4,?5)",
                (
                    &id,
                    &scan.repertoire,
                    rev,
                    serde_json::to_string(&scan)?,
                    report,
                ),
            )?;
        }
        Ok(json!({"id":id}))
    })
    .await
}

#[derive(Default, Deserialize)]
pub struct Page {
    #[serde(default)]
    offset: usize,
}
pub async fn read(
    State(app): State<App>,
    Path(id): Path<String>,
    axum::extract::Query(page): axum::extract::Query<Page>,
) -> ViewResult {
    optional_view(app, optional_read::Reader::Coverage, move |c| {
        let tx = c.transaction()?;
        let (scan, rev, forecast) = load(&tx, &id)?;
        let mut seen = HashSet::new();
        let mut query =
            tx.prepare_cached("SELECT EXISTS(SELECT 1 FROM learning WHERE card_id=?1)")?;
        for card in forecast
            .paths
            .iter()
            .flat_map(|(path, _)| path)
            .collect::<HashSet<_>>()
        {
            if query.query_row([card], |r| r.get::<_, bool>(0))? {
                seen.insert(card);
            }
        }
        let mut authored = 0.0;
        let mut studied = 0.0;
        for (path, mass) in &forecast.paths {
            authored += mass;
            if path.iter().all(|card| seen.contains(card)) {
                studied += mass;
            }
        }
        let total = forecast.gaps.len();
        let gaps: Vec<_> = forecast
            .gaps
            .into_iter()
            .skip(page.offset)
            .take(30)
            .collect();
        Ok(json!({
            "id":id, "stale":revision(&tx,&scan.repertoire)?!=rev, "scan":scan,
            "model":forecast.model.label(), "model_superseded":forecast.model != ForecastModel::Maia3,
            "authored":authored, "studied":studied, "ended":forecast.ended,
            "conflict":forecast.conflict, "unresolved":forecast.unresolved,
            "gaps":gaps, "total":total
        }))
    })
    .await
}

pub fn selected(c: &Connection, id: &str, index: usize) -> Result<(Scan, i64, Gap)> {
    let (scan, rev, forecast) = load(c, id)?;
    let gap = forecast
        .gaps
        .into_iter()
        .nth(index)
        .context("Unknown gap")?;
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
        ensure!(forecast.model == ForecastModel::Maia3, SupersededModel);
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
            .err()
            .expect("old report accepted")
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
