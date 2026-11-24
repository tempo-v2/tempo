mod catalog;
mod chess;
mod db;
mod games;
mod jobs;
mod legacy;
mod study;

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path as UrlPath, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use db::Database;
use fs2::FileExt;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};
use tower_http::services::{ServeDir, ServeFile};

type App = Arc<PathBuf>;
type ApiResult = std::result::Result<Json<Value>, ApiError>;

struct ApiError(anyhow::Error);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let Some(error) = self.0.downcast_ref::<study::Conflict>() {
            return (
                StatusCode::CONFLICT,
                Json(json!({"code":error.code(),"error":error.to_string()})),
            )
                .into_response();
        }
        let status = if self.0.downcast_ref::<rusqlite::Error>().is_some() {
            eprintln!("Tempo database operation failed: {:#}", self.0);
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::BAD_REQUEST
        };
        let message = if status.is_server_error() {
            "The database operation failed. Try again.".into()
        } else {
            self.0.to_string()
        };
        (status, Json(json!({"error":message}))).into_response()
    }
}

async fn with_db<T: Serialize + Send + 'static>(
    db: App,
    f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
) -> ApiResult {
    let result = tokio::task::spawn_blocking(move || {
        let mut connection = Database::open(&db)?;
        f(&mut connection)
    })
    .await
    .map_err(|e| ApiError(e.into()))?
    .map_err(ApiError)?;
    Ok(Json(
        serde_json::to_value(result).map_err(|e| ApiError(e.into()))?,
    ))
}

#[derive(Default, Deserialize)]
struct Page {
    #[serde(default)]
    after: String,
}

async fn openings(State(db): State<App>, Query(q): Query<Page>) -> ApiResult {
    with_db(db, move |c| catalog::openings(c, &q.after)).await
}
async fn save_opening(
    State(db): State<App>,
    Json(input): Json<catalog::OpeningInput>,
) -> ApiResult {
    with_db(db, move |c| {
        let t = c.transaction()?;
        let id = catalog::save_opening(&t, &input, None)?;
        t.commit()?;
        Ok(json!({"id":id}))
    })
    .await
}

#[derive(Deserialize)]
struct PgnInput {
    pgn: String,
    name: Option<String>,
}

async fn preview_opening(Json(input): Json<PgnInput>) -> ApiResult {
    let result = (|| -> Result<_> {
        ensure!(input.pgn.len() <= jobs::MAX_PGN, "PGN exceeds 64 MiB");
        let mut games = chess::parse_pgn(&input.pgn, false, 1)?;
        Ok(games.remove(0)?.main)
    })()
    .map_err(ApiError)?;
    Ok(Json(json!(result)))
}

async fn tactics(State(db): State<App>, Query(q): Query<Page>) -> ApiResult {
    with_db(db, move |c| catalog::tactics(c, &q.after)).await
}
async fn save_tactic(State(db): State<App>, Json(input): Json<PgnInput>) -> ApiResult {
    with_db(db, move |c| {
        ensure!(input.pgn.len() <= jobs::MAX_PGN, "PGN exceeds 64 MiB");
        let mut pgn = chess::parse_pgn(&input.pgn, true, 1)?;
        let puzzle = pgn.remove(0)?;
        let name = input.name.as_deref().context("enter a puzzle name")?;
        let mut answers = vec![puzzle.main.moves[0].clone()];
        answers.extend(
            puzzle
                .variations
                .iter()
                .filter_map(|r| r.moves.first().cloned()),
        );
        // Only alternatives branching at the first move differ from main[0].
        let t = c.transaction()?;
        let id = catalog::publish_tactic(
            &t,
            catalog::TacticInput {
                name,
                route: &puzzle.main,
                answers: &answers,
                source: None,
            },
        )?;
        t.commit()?;
        Ok(json!({"id":id}))
    })
    .await
}

async fn archives(State(db): State<App>, Query(q): Query<Page>) -> ApiResult {
    with_db(db, move |c| games::archive(c, &q.after, 50)).await
}
async fn read_game(State(db): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult {
    with_db(db, move |c| games::read(c, &id)).await
}
async fn job_views(State(db): State<App>) -> ApiResult {
    with_db(db, move |c| jobs::views(c)).await
}
async fn start_job(State(db): State<App>, Json(input): Json<jobs::StartJob>) -> ApiResult {
    with_db(db, move |c| Ok(json!({"id":jobs::start(c,input)?}))).await
}
async fn retry_job(
    State(db): State<App>,
    UrlPath(id): UrlPath<i64>,
    Json(_): Json<Value>,
) -> ApiResult {
    with_db(db, move |c| {
        jobs::retry(c, id)?;
        Ok(json!({"id":id}))
    })
    .await
}
async fn settings(State(db): State<App>) -> ApiResult {
    with_db(db, move |c| jobs::settings(c)).await
}
async fn save_settings(
    State(db): State<App>,
    Json(names): Json<std::collections::BTreeMap<String, String>>,
) -> ApiResult {
    with_db(db, move |c| {
        jobs::save_settings(c, names)?;
        jobs::settings(c)
    })
    .await
}
async fn current(State(db): State<App>) -> ApiResult {
    with_db(db, move |c| study::current(c)).await
}
async fn next_review(State(db): State<App>, Json(input): Json<study::Next>) -> ApiResult {
    with_db(db, move |c| study::next(c, input)).await
}
async fn answer(State(db): State<App>, Json(input): Json<study::Answer>) -> ApiResult {
    with_db(db, move |c| study::answer(c, input)).await
}
async fn context(State(db): State<App>, UrlPath(id): UrlPath<i64>) -> ApiResult {
    with_db(db, move |c| study::context(c, id)).await
}

async fn loopback(State(port): State<u16>, req: Request, next: Next) -> Response {
    let hostname = req.headers().get("host").and_then(|h| h.to_str().ok());
    let host_ok = hostname.is_some_and(|host| {
        host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
    });
    let origin_ok = req.headers().get("origin").is_none_or(|origin| {
        origin
            .to_str()
            .ok()
            .is_some_and(|origin| hostname.is_some_and(|host| origin == format!("http://{host}")))
    });
    if !host_ok || !origin_ok {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(req).await
}

async fn serve(db: PathBuf, port: u16, without_worker: bool) -> Result<()> {
    Database::init(&db)?;
    let db = db.canonicalize()?;
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/dist");
    ensure!(
        path.join("index.html").is_file(),
        "build the UI first with npm --prefix ui ci and npm --prefix ui run build"
    );
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let port = listener.local_addr()?.port();
    let _worker_lock = if without_worker {
        None
    } else {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(db.with_extension("worker.lock"))?;
        lock.try_lock_exclusive()
            .context("another worker owns this database")?;
        let worker_db = db.clone();
        std::thread::Builder::new()
            .name("games-worker".into())
            .spawn(move || {
                if let Err(error) = jobs::worker(&worker_db) {
                    eprintln!("Games worker stopped: {error:#}");
                }
            })?;
        Some(lock)
    };
    let pgn_body_limit = DefaultBodyLimit::max(jobs::MAX_PGN * 2 + 4096);
    let app = Router::new()
        .route(
            "/api/health",
            get(|| async { Json(json!({"status":"ready","profile":"lean"})) }),
        )
        .route("/api/openings", get(openings).post(save_opening))
        .route(
            "/api/openings/pgn",
            post(preview_opening).layer(pgn_body_limit),
        )
        .route(
            "/api/tactics",
            get(tactics).post(save_tactic).layer(pgn_body_limit),
        )
        .route("/api/games", get(archives))
        .route("/api/games/archive/{id}", get(read_game))
        .route(
            "/api/games/jobs",
            get(job_views).post(start_job).layer(pgn_body_limit),
        )
        .route("/api/games/jobs/{id}/retry", post(retry_job))
        .route("/api/settings", get(settings).post(save_settings))
        .route("/api/study", get(current))
        .route("/api/study/next", post(next_review))
        .route("/api/study/answer", post(answer))
        .route("/api/study/{id}/context", get(context))
        .route_service("/", ServeFile::new(path.join("index.html")))
        .nest_service("/assets", ServeDir::new(path.join("assets")))
        .layer(middleware::from_fn_with_state(port, loopback))
        .with_state(Arc::new(db));
    println!("Tempo at http://127.0.0.1:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.as_slice() {
        [command, db] if command == "serve" => serve(db.into(), 41823, false).await,
        [command, db, port] if command == "serve" => serve(db.into(), port.parse()?, false).await,
        [command, db, port, flag] if command == "serve" && flag == "--no-worker" => {
            serve(db.into(), port.parse()?, true).await
        }
        [command, source, dest] if command == "import-openings" => {
            legacy::import(Path::new(source), Path::new(dest))
        }
        _ => anyhow::bail!(
            "usage: tempo serve NEW_DB [PORT [--no-worker]] | tempo import-openings OLD_DB NEW_DB"
        ),
    }
}
