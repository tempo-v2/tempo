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
    gaps: Vec<Gap>,
    pub reach: HashMap<String, f64>,
    paths: Vec<(Vec<String>, f64)>,
    ended: f64,
    conflict: f64,
    unresolved: f64,
}

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
        "maia2-rapid-1500-coverage-v1",
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
        .join("Library/Application Support/Tempo Lean/Maia");
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
    Ok(serde_json::from_slice(&output.stdout)?)
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

pub async fn read(State(app): State<App>, Path(id): Path<String>) -> ViewResult {
    optional_view(app, optional_read::Reader::Coverage,move|c|{
        let tx=c.transaction()?;
        let (scan,rev,forecast)=load(&tx,&id)?;
        let mut seen=HashSet::new();
        let mut q=tx.prepare_cached("SELECT EXISTS(SELECT 1 FROM learning WHERE card_id=?1)")?;
        for card in forecast.paths.iter().flat_map(|(path,_)|path).collect::<HashSet<_>>() {
            if q.query_row([card],|r|r.get::<_,bool>(0))? {seen.insert(card);}
        }
        let studied:f64=forecast.paths.iter().filter(|(p,_)|p.iter().all(|id|seen.contains(id))).map(|(_,m)|m).sum();
        let authored:f64=forecast.paths.iter().map(|(_,m)|m).sum();
        Ok(json!({"id":id,"stale":revision(&tx,&scan.repertoire)?!=rev,"scan":scan,"authored":authored,"studied":studied,"ended":forecast.ended,"conflict":forecast.conflict,"unresolved":forecast.unresolved,"gaps":forecast.gaps}))
    }).await
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
