mod capture_publication;
mod catalog;
mod chess;
mod coverage;
mod db;
mod full_line;
mod full_line_store;
mod game_feedback;
mod game_ir;
mod game_pipeline;
mod games;
mod gap_edit;
mod gap_pgn;
mod job_store;
mod jobs;
mod learning;
mod legacy;
mod lichess_catalog;
mod lichess_play;
mod opening_engine;
mod opening_rank;
mod opening_replacement;
mod optional_read;
mod providers;
mod queue;
mod queue_store;
mod repertoire_similarity;
mod review;
mod scanner;
mod study;
mod study_store;
mod tactic_qualification;
mod tactic_worker_protocol;
mod tactic_worker_socket;
mod uploads;
mod writer;

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequest, Path as UrlPath, Query, Request, State},
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
    io::{self, Read},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, SemaphorePermit};
use tower_http::services::{ServeDir, ServeFile};

type App = Arc<Runtime>;
struct Runtime {
    database: PathBuf,
    opening_engine: Option<opening_engine::Endpoint>,
    shutdown: tokio::sync::watch::Sender<bool>,
    writer: writer::Writer,
    staging: uploads::Staging,
    content_preparation: Arc<Semaphore>,
    games_upload_staging: Arc<Semaphore>,
    games_pgn_receive: Arc<Semaphore>,
    tactics_pgn_receive: Arc<Semaphore>,
}

impl Runtime {
    fn new(database: PathBuf, writer: writer::Writer, staging: uploads::Staging) -> Self {
        Self {
            database,
            opening_engine: None,
            shutdown: tokio::sync::watch::channel(false).0,
            writer,
            staging,
            content_preparation: Arc::new(Semaphore::new(2)),
            games_upload_staging: Arc::new(Semaphore::new(2)),
            games_pgn_receive: Arc::new(Semaphore::new(1)),
            tactics_pgn_receive: Arc::new(Semaphore::new(1)),
        }
    }
}
type ApiResult = std::result::Result<Json<Value>, ApiError>;

struct ApiError(anyhow::Error);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let Some(error) = self.0.downcast_ref::<repertoire_similarity::Error>() {
            use repertoire_similarity::Error::*;
            let (status, message) = match error {
                InvalidRequest => (StatusCode::BAD_REQUEST, "Invalid position or repertoire."),
                InvalidSnapshot => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "The saved repertoire could not be read.",
                ),
                WorkLimit => (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "This repertoire exceeds the similarity search limit.",
                ),
                Unavailable => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Similar moves are temporarily unavailable.",
                ),
            };
            return (status, Json(json!({"code":error,"error":message}))).into_response();
        }

        if self.0.is::<gap_edit::Stale>() {
            return (
                StatusCode::CONFLICT,
                Json(json!({"code":"stale_gap","error":self.0.to_string()})),
            )
                .into_response();
        }
        if let Some(error) = self.0.downcast_ref::<coverage::ScanFailure>() {
            return (
                error.status(),
                Json(json!({"code":error,"error":error.to_string()})),
            )
                .into_response();
        }
        if self.0.is::<coverage::SupersededModel>() {
            return (
                StatusCode::CONFLICT,
                Json(json!({"code":"coverage_model_superseded","error":self.0.to_string()})),
            )
                .into_response();
        }
        if self.0.is::<gap_edit::Interrupted>() {
            return (
                StatusCode::CONFLICT,
                Json(json!({"code":"gap_save_interrupted","error":self.0.to_string()})),
            )
                .into_response();
        }
        if let Some(error) = self
            .0
            .downcast_ref::<opening_replacement::PreviewUnavailable>()
        {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "code":"replacement_preview_unavailable", "reason":error.reason(), "error":error.to_string()
                })),
            ).into_response();
        }
        if self.0.is::<opening_replacement::OverBudget>() {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({"code":"replacement_over_budget","error":self.0.to_string()})),
            )
                .into_response();
        }
        if self.0.is::<catalog::OverBudget>() {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({
                    "code":"content_over_budget", "error":self.0.to_string()
                })),
            )
                .into_response();
        }
        if self.0.is::<review::Preparing>() {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "code":"study_preparing", "error":self.0.to_string()
                })),
            )
                .into_response();
        }
        if self.0.is::<optional_read::Unavailable>() {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"code":"view_unavailable","error":self.0.to_string()})),
            )
                .into_response();
        }
        if self.0.is::<writer::Unavailable>() {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "code":"writer_busy", "error":self.0.to_string()
                })),
            )
                .into_response();
        }
        if self.0.is::<game_feedback::QueueFull>() {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"code":"comparison_queue_full","error":self.0.to_string()})),
            )
                .into_response();
        }
        if self.0.is::<uploads::Unavailable>() {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"code":"staging_unavailable","error":self.0.to_string()})),
            )
                .into_response();
        }
        if let Some(error) = self.0.downcast_ref::<study::Conflict>() {
            return (
                StatusCode::CONFLICT,
                Json(json!({"code":error.code(),"error":error.to_string()})),
            )
                .into_response();
        }
        if self.0.is::<catalog::OpeningConflict>() || self.0.is::<gap_edit::Conflicts>() {
            return (
                StatusCode::CONFLICT,
                Json(json!({"code":"opening_reply_conflict","error":self.0.to_string()})),
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

async fn write_db<T: Serialize + Send + 'static>(
    app: App,
    apply: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
) -> ApiResult {
    let reply = app.writer.submit(false, apply).map_err(ApiError)?;
    let value = reply.wait().await.map_err(ApiError)?;
    Ok(Json(
        serde_json::to_value(value).map_err(|error| ApiError(error.into()))?,
    ))
}

async fn prepare<T: Send + 'static>(
    slots: &Arc<Semaphore>,
    busy: fn() -> ApiError,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T, ApiError> {
    let permit = slots.clone().try_acquire_owned().map_err(|_| busy())?;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .map_err(|_| busy())?
        .map_err(|error| ApiError(error.into()))?
        .map_err(ApiError)
}

#[derive(Default, Deserialize)]
struct Page {
    #[serde(default)]
    after: String,
}

async fn openings(State(db): State<App>, Query(q): Query<Page>) -> ViewResult {
    optional_view(db, optional_read::Reader::Openings, move |c| {
        catalog::openings(c, &q.after)
    })
    .await
}
#[derive(Deserialize)]
struct SaveOpening {
    #[serde(flatten)]
    opening: catalog::OpeningInput,
    source_opening: Option<String>,
    destination: Option<OpeningDestination>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum OpeningDestination {
    New,
    Existing { repertoire_id: String },
}

#[derive(Default, Deserialize)]
struct RepertoirePage {
    #[serde(default)]
    after: String,
    #[serde(default)]
    color: String,
}

async fn repertoires(State(db): State<App>, Query(page): Query<RepertoirePage>) -> ViewResult {
    optional_view(db, optional_read::Reader::Repertoires, move |c| {
        catalog::repertoires(c, &page.after, &page.color)
    })
    .await
}

async fn repertoire(
    State(db): State<App>,
    UrlPath((id, color)): UrlPath<(String, String)>,
) -> ViewResult {
    optional_view(db, optional_read::Reader::RepertoireChoice, move |c| {
        catalog::repertoire(c, &id, &color)
    })
    .await
}

fn apply_opening(
    db: &mut Connection,
    plan: &catalog::OpeningPlan,
    destination: Option<&OpeningDestination>,
    source: Option<&str>,
    online: bool,
) -> Result<String> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let repertoire = match (destination, source) {
        (Some(OpeningDestination::Existing { repertoire_id }), None) => {
            catalog::existing_repertoire(&tx, repertoire_id, &plan.color)?;
            Some(repertoire_id.clone())
        }
        (Some(OpeningDestination::New) | None, None) => None,
        (None, Some(source)) => Some(catalog::branch_repertoire(&tx, source, &plan.color)?),
        (Some(_), Some(_)) => anyhow::bail!("Choose one opening destination."),
    };
    let id = catalog::apply_opening(&tx, plan, repertoire.as_deref(), online)?;
    tx.commit()?;
    Ok(id)
}

async fn save_opening(State(db): State<App>, Json(input): Json<SaveOpening>) -> ApiResult {
    let plan = prepare(
        &db.content_preparation,
        || ApiError(writer::Unavailable.into()),
        move || catalog::prepare_opening(&input.opening),
    )
    .await?;
    write_db(db, move |c| {
        let id = apply_opening(
            c,
            &plan,
            input.destination.as_ref(),
            input.source_opening.as_deref(),
            true,
        )?;
        Ok(json!({"id":id}))
    })
    .await
}

#[derive(Deserialize)]
struct PgnInput {
    pgn: String,
    name: Option<String>,
}

static OPENING_PREVIEW: Semaphore = Semaphore::const_new(1);
const PGN_DEADLINE: Duration = Duration::from_secs(10);
const PGN_BODY_LIMIT: usize = jobs::MAX_PGN * 2 + 4096;

async fn preview_opening(request: Request) -> Response {
    let deadline = Instant::now() + PGN_DEADLINE;
    let Ok(permit) = OPENING_PREVIEW.try_acquire() else {
        return ApiError(optional_read::Unavailable.into()).into_response();
    };
    let request = match buffer_pgn(request, deadline).await {
        Ok(request) => request,
        Err(status) => return status.into_response(),
    };
    let runtime = tokio::runtime::Handle::current();
    let reply = run_preview(permit, deadline, move || {
        // The bounded body is already in memory. Decode it on the preview worker.
        let Json(input) = match runtime.block_on(Json::<PgnInput>::from_request(request, &())) {
            Ok(input) => input,
            Err(error) => return Ok(error.into_response()),
        };
        ensure!(input.pgn.len() <= jobs::MAX_PGN, "PGN exceeds 64 MiB");
        ensure!(!input.pgn.trim().is_empty(), "enter a PGN");
        let route = chess::preview_pgn(PreviewInput {
            bytes: input.pgn.as_bytes(),
            deadline,
        })?;
        Ok(Json(route).into_response())
    })
    .await;
    reply.unwrap_or_else(|error| ApiError(error).into_response())
}

struct PreviewInput<'a> {
    bytes: &'a [u8],
    deadline: Instant,
}

impl Read for PreviewInput<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.bytes.read(buffer)
    }
}

async fn run_preview<T: Send + 'static>(
    permit: SemaphorePermit<'static>,
    deadline: Instant,
    parse: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        ensure!(Instant::now() < deadline, optional_read::Unavailable);
        let result = parse();
        ensure!(Instant::now() < deadline, optional_read::Unavailable);
        result
    });
    tokio::time::timeout_at(deadline.into(), task)
        .await
        .map_err(|_| optional_read::Unavailable)?
        .map_err(|_| optional_read::Unavailable)?
}

async fn tactics(State(db): State<App>, Query(q): Query<Page>) -> ViewResult {
    optional_view(db, optional_read::Reader::Tactics, move |c| {
        catalog::tactics(c, &q.after)
    })
    .await
}
fn prepare_puzzle(input: PgnInput) -> Result<catalog::TacticPlan> {
    ensure!(input.pgn.len() <= jobs::MAX_PGN, "PGN exceeds 64 MiB");
    let puzzle = chess::parse_pgn(&input.pgn, true, 1)?.remove(0)?;
    let name = input.name.as_deref().context("enter a puzzle name")?;
    let mut answers = vec![puzzle.main.moves[0].clone()];
    answers.extend(
        puzzle
            .variations
            .iter()
            .filter_map(|route| route.moves.first().cloned()),
    );
    catalog::prepare_tactic(catalog::TacticInput {
        name,
        route: &puzzle.main,
        answers: &answers,
        source: None,
    })
}

fn apply_puzzle(db: &mut Connection, plan: &catalog::TacticPlan) -> Result<String> {
    let tx = db.transaction()?;
    let id = catalog::apply_tactic(&tx, plan)?;
    tx.commit()?;
    Ok(id)
}

async fn save_tactic(State(db): State<App>, Json(input): Json<PgnInput>) -> ApiResult {
    let plan = prepare(
        &db.content_preparation,
        || ApiError(writer::Unavailable.into()),
        move || prepare_puzzle(input),
    )
    .await?;
    write_db(db, move |c| Ok(json!({"id":apply_puzzle(c,&plan)?}))).await
}

async fn archives(State(db): State<App>, Query(q): Query<Page>) -> ViewResult {
    optional_view(db, optional_read::Reader::GamesArchive, move |c| {
        games::archive(c, &q.after, 50)
    })
    .await
}
async fn read_game(State(db): State<App>, UrlPath(id): UrlPath<String>) -> ViewResult {
    optional_view(db, optional_read::Reader::GameReplay, move |c| {
        games::replay(c, &id)
    })
    .await
}
async fn feedback_repertoires(
    State(db): State<App>,
    UrlPath(game): UrlPath<String>,
    Query(query): Query<game_feedback::ChoicesQuery>,
) -> ViewResult {
    optional_view(db, optional_read::Reader::FeedbackRepertoires, move |c| {
        game_feedback::choices(c, game, query)
    })
    .await
}
async fn feedback_view(
    State(db): State<App>,
    UrlPath((game, repertoire)): UrlPath<(String, String)>,
    Query(query): Query<game_feedback::ReadQuery>,
) -> ViewResult {
    optional_view(db, optional_read::Reader::GameFeedback, move |c| {
        game_feedback::read(c, game_feedback::Pair { game, repertoire }, query)
    })
    .await
}
async fn request_feedback(
    State(db): State<App>,
    UrlPath((game, repertoire)): UrlPath<(String, String)>,
    Json(input): Json<game_feedback::Request>,
) -> ApiResult {
    write_db(db, move |c| {
        game_feedback::request(c, game_feedback::Pair { game, repertoire }, input)
    })
    .await
}
async fn job_views(State(db): State<App>) -> ViewResult {
    optional_view(db, optional_read::Reader::GamesStatus, |c| {
        job_store::views(c)
    })
    .await
}
async fn start_job(State(db): State<App>, Json(input): Json<jobs::StartJob>) -> ApiResult {
    write_db(db, move |c| {
        Ok(json!({"id":job_store::start(c, jobs::NewJob::Command(input))?}))
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GamesUpload {
    username: String,
    pgn: String,
}

async fn upload_games(State(db): State<App>, Json(input): Json<GamesUpload>) -> ApiResult {
    let username = jobs::valid_user(&input.username)
        .map_err(ApiError)?
        .to_owned();
    let staging = db.staging.clone();
    let staged = prepare(
        &db.games_upload_staging,
        || ApiError(uploads::Unavailable.into()),
        move || staging.manual(&input.pgn),
    )
    .await?;
    write_db(db, move |c| {
        let id = job_store::start(
            c,
            jobs::NewJob::Upload {
                username,
                file: staged.id().to_owned(),
            },
        )?;
        Ok(json!({"id":id}))
    })
    .await
}
async fn retry_job(
    State(db): State<App>,
    UrlPath(id): UrlPath<i64>,
    Json(_): Json<Value>,
) -> ApiResult {
    write_db(db, move |c| {
        job_store::retry(c, id)?;
        Ok(json!({"id":id}))
    })
    .await
}
async fn settings(State(db): State<App>) -> ViewResult {
    optional_view(db, optional_read::Reader::ProviderSettings, move |c| {
        job_store::settings(c)
    })
    .await
}
async fn save_settings(
    State(db): State<App>,
    Json(names): Json<std::collections::BTreeMap<String, String>>,
) -> ApiResult {
    write_db(db, move |c| {
        job_store::save_settings(c, names)?;
        job_store::settings(c)
    })
    .await
}
async fn new_opening_settings(State(db): State<App>) -> ViewResult {
    optional_view(db, optional_read::Reader::OpeningSettings, |c| {
        queue_store::new_opening_settings(c)
    })
    .await
}

async fn save_new_opening_settings(
    State(db): State<App>,
    Json(input): Json<queue::NewOpeningSettings>,
) -> ApiResult {
    write_db(db, |c| queue_store::save_new_opening_settings(c, input)).await
}

async fn current(State(db): State<App>) -> ViewResult {
    optional_view(db, optional_read::Reader::CurrentStudy, move |c| {
        study_store::current(c)
    })
    .await
}
async fn study_counts(State(db): State<App>) -> impl IntoResponse {
    let response = optional_view(db, optional_read::Reader::Counts, review::counts)
        .await
        .into_response();
    ([(axum::http::header::CACHE_CONTROL, "no-store")], response)
}
async fn next_review(State(db): State<App>, Json(input): Json<study::Next>) -> ApiResult {
    write_db(db, move |c| review::next(c, input)).await
}
async fn answer(State(db): State<App>, Json(input): Json<study::AnswerWire>) -> ApiResult {
    write_db(db, move |c| review::answer(c, input.into())).await
}
async fn pack_index(State(db): State<App>) -> ViewResult {
    optional_view(db, optional_read::Reader::LichessPacks, |c| {
        lichess_catalog::index(c)
    })
    .await
}
#[derive(Deserialize)]
struct PackCursor {
    #[serde(default)]
    after: i64,
}
async fn pack_theme(
    State(db): State<App>,
    UrlPath(theme): UrlPath<String>,
    Query(page): Query<PackCursor>,
) -> ViewResult {
    optional_view(db, optional_read::Reader::LichessPacks, move |c| {
        lichess_catalog::theme(c, &theme, page.after)
    })
    .await
}
async fn pack_members(
    State(db): State<App>,
    UrlPath((theme, band)): UrlPath<(String, i64)>,
) -> ViewResult {
    optional_view(db, optional_read::Reader::LichessMembers, move |c| {
        lichess_catalog::members(c, &theme, band)
    })
    .await
}
async fn pack_pending(State(db): State<App>, Query(page): Query<PackCursor>) -> ViewResult {
    optional_view(db, optional_read::Reader::LichessPlays, move |c| {
        full_line_store::pending(c, page.after)
    })
    .await
}
async fn pack_resume(State(db): State<App>, UrlPath(play): UrlPath<i64>) -> ViewResult {
    optional_view(db, optional_read::Reader::LichessPlays, move |c| {
        full_line_store::resume_tactics(c, play)
    })
    .await
}
async fn pack_start(State(db): State<App>, Json(input): Json<lichess_play::Start>) -> ApiResult {
    write_db(db, move |c| lichess_play::start(c, input)).await
}
async fn pack_move(State(db): State<App>, Json(input): Json<full_line::Move>) -> ApiResult {
    write_db(db, move |c| {
        review::move_full_line(
            c,
            input,
            review::FullLineSurface::Tactics {
                first_card: lichess_play::first_card,
            },
        )
    })
    .await
}
async fn study_puzzle_move(State(db): State<App>, Json(input): Json<full_line::Move>) -> ApiResult {
    write_db(db, move |c| {
        review::move_full_line(c, input, review::FullLineSurface::Study)
    })
    .await
}
type ViewResult = std::result::Result<Response, ApiError>;

async fn optional_view<T: Serialize + 'static>(
    db: App,
    reader: optional_read::Reader,
    work: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
) -> ViewResult {
    let bytes = optional_read::read(reader, db.database.clone(), work)
        .await
        .map_err(ApiError)?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        bytes,
    )
        .into_response())
}

async fn context(State(db): State<App>, UrlPath(id): UrlPath<i64>) -> ViewResult {
    optional_view(db, optional_read::Reader::StudyContext, move |c| {
        let source = study_store::context_source(c, id)?;
        let mut context = source.context;
        if let (Some(repertoire), Some(version)) = (source.repertoire_id, source.answer_version)
            && let Some((game, ply, played)) =
                games::mismatch_context(c, &source.position_id, &repertoire, &version)?
        {
            context.game_id = Some(game);
            context.ply = Some(ply);
            context.played = Some(played);
        }
        Ok(context)
    })
    .await
}

async fn opponent_move(State(db): State<App>, UrlPath(id): UrlPath<i64>) -> ViewResult {
    optional_view(db, optional_read::Reader::StudyContext, move |c| {
        let Some(anchor) = study_store::game_anchor(c, id)? else {
            return Ok(None);
        };
        games::preceding_move(c, &anchor.game_id, anchor.ply, &anchor.fen)
    })
    .await
}

async fn receive_pgn(State(slot): State<Arc<Semaphore>>, request: Request, next: Next) -> Response {
    let Ok(_permit) = slot.try_acquire() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "code": "pgn_receive_busy",
                "error": "This PGN intake is busy. Retry the same request."
            })),
        )
            .into_response();
    };
    match buffer_pgn(request, Instant::now() + PGN_DEADLINE).await {
        Ok(request) => next.run(request).await,
        Err(status) => status.into_response(),
    }
}

async fn buffer_pgn(request: Request, deadline: Instant) -> Result<Request, StatusCode> {
    let (parts, body) = request.into_parts();
    let bytes =
        match tokio::time::timeout_at(deadline.into(), axum::body::to_bytes(body, PGN_BODY_LIMIT))
            .await
        {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => return Err(StatusCode::PAYLOAD_TOO_LARGE),
            Err(_) => return Err(StatusCode::REQUEST_TIMEOUT),
        };
    Ok(Request::from_parts(parts, axum::body::Body::from(bytes)))
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

fn lock_database(db: &Path) -> Result<std::fs::File> {
    let path = if db.exists() {
        db.canonicalize()?
    } else {
        db.to_owned()
    };
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("worker.lock"))?;
    lock.try_lock_exclusive()
        .context("another service owns this database")?;
    Ok(lock)
}

async fn serve(db: PathBuf, port: u16, without_worker: bool) -> Result<()> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/dist");
    ensure!(
        path.join("index.html").is_file(),
        "build the UI first with npm --prefix ui ci and npm --prefix ui run build"
    );
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let port = listener.local_addr()?.port();
    let db = if db.exists() {
        db.canonicalize()?
    } else {
        let directory = db
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        directory
            .canonicalize()?
            .join(db.file_name().context("enter a database file name")?)
    };
    // Fence the previous worker before any schema or data writes.
    let database_lock = lock_database(&db)?;
    let connection = Database::init(&db)?;
    let writer = writer::Writer::start(connection, Some(database_lock))?;
    let plan_writer = writer.clone();
    std::thread::Builder::new()
        .name("tempo-prepare-study".into())
        .spawn(move || {
            loop {
                match plan_writer.background(queue_store::seed_batch) {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(error) => {
                        eprintln!("Study queue preparation failed: {error:#}");
                        std::thread::sleep(std::time::Duration::from_secs(2));
                    }
                }
            }
        })?;
    let staging = uploads::Staging::start(&db)?;
    if !without_worker {
        let worker_db = db.clone();
        let worker_writer = writer.clone();
        let worker_staging = staging.clone();
        let runtime = tokio::runtime::Handle::current();
        std::thread::Builder::new()
            .name("games-worker".into())
            .spawn(move || {
                let _runtime = runtime.enter();
                if let Err(error) = jobs::worker(&worker_db, worker_writer, worker_staging) {
                    eprintln!("Games worker stopped: {error:#}");
                }
            })?;
    }
    let mut runtime = Runtime::new(db, writer, staging);
    match tactic_worker_socket::start(&runtime.database, runtime.writer.clone()) {
        Ok(owner) => {
            runtime.opening_engine = Some(opening_engine::Endpoint::new(&runtime.database, owner));
        }
        Err(error) => eprintln!("Tactic worker handoff unavailable: {error:#}"),
    }
    let shutdown = runtime.shutdown.clone();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let app = router(&path, port, Arc::new(runtime));
    println!("Tempo at http://127.0.0.1:{port}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
            shutdown.send_replace(true);
        })
        .await?;
    Ok(())
}

fn router(path: &Path, port: u16, app: App) -> Router {
    let pgn_body_limit = DefaultBodyLimit::max(PGN_BODY_LIMIT);
    Router::new()
        .route(
            "/api/health",
            get(|| async { Json(json!({"status":"ready","profile":"lean"})) }),
        )
        .route("/api/coverage", post(coverage::start))
        .route("/api/coverage/{id}", get(coverage::read))
        .route("/api/coverage/{id}/gaps", get(coverage::gaps))
        .route("/api/coverage/{id}/studied", get(coverage::studied))
        .route("/api/coverage/gaps/review", post(coverage::review::review))
        .route("/api/coverage/{id}/apply", post(coverage::apply))
        .route(
            "/api/coverage/preview",
            post(gap_edit::preview).layer(DefaultBodyLimit::max(800 * 1024)),
        )
        .route(
            "/api/coverage/correct",
            post(gap_edit::correct).layer(DefaultBodyLimit::max(800 * 1024)),
        )
        .route(
            "/api/coverage/replace",
            post(gap_edit::replace).layer(DefaultBodyLimit::max(800 * 1024)),
        )
        .route(
            "/api/coverage/save",
            post(gap_edit::save).layer(DefaultBodyLimit::max(800 * 1024)),
        )
        .route("/api/coverage/save/{command}", get(gap_edit::progress))
        .route("/api/openings", get(openings).post(save_opening))
        .route("/api/openings/repertoires", get(repertoires))
        .route("/api/openings/repertoires/{id}/{color}", get(repertoire))
        .route(
            "/api/openings/similar-moves",
            post(repertoire_similarity::browse),
        )
        .route("/api/openings/engine-moves", post(opening_engine::browse))
        .route(
            "/api/openings/pgn",
            post(preview_opening).layer(pgn_body_limit),
        )
        .route(
            "/api/tactics",
            get(tactics).merge(post(save_tactic).layer(pgn_body_limit).layer(
                middleware::from_fn_with_state(app.tactics_pgn_receive.clone(), receive_pgn),
            )),
        )
        .route("/api/games", get(archives))
        .route("/api/games/archive/{id}", get(read_game))
        .route(
            "/api/games/archive/{id}/repertoires",
            get(feedback_repertoires),
        )
        .route(
            "/api/games/archive/{id}/feedback/{repertoire}",
            get(feedback_view).post(request_feedback),
        )
        .route("/api/games/jobs", get(job_views).post(start_job))
        .route(
            "/api/games/uploads",
            post(upload_games)
                .layer(pgn_body_limit)
                .layer(middleware::from_fn_with_state(
                    app.games_pgn_receive.clone(),
                    receive_pgn,
                )),
        )
        .route("/api/games/jobs/{id}/retry", post(retry_job))
        .route("/api/settings", get(settings).post(save_settings))
        .route(
            "/api/settings/new-openings",
            get(new_opening_settings).post(save_new_opening_settings),
        )
        .route("/api/tactics/packs", get(pack_index))
        .route("/api/tactics/packs/{theme}", get(pack_theme))
        .route("/api/tactics/packs/{theme}/{band}", get(pack_members))
        .route("/api/tactics/plays", get(pack_pending).post(pack_start))
        .route("/api/tactics/plays/{play}", get(pack_resume))
        .route("/api/tactics/plays/move", post(pack_move))
        .route("/api/study/puzzle-move", post(study_puzzle_move))
        .route("/api/study", get(current))
        .route("/api/study/counts", get(study_counts))
        .route("/api/study/next", post(next_review))
        .route("/api/study/answer", post(answer))
        .route("/api/study/{id}/context", get(context))
        .route("/api/study/{id}/opponent-move", get(opponent_move))
        .route_service("/", ServeFile::new(path.join("index.html")))
        .nest_service("/assets", ServeDir::new(path.join("assets")))
        .layer(middleware::from_fn_with_state(port, loopback))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(app)
}

fn print_similarity(result: Result<impl Serialize, repertoire_similarity::Error>) -> Result<()> {
    let value = match result {
        Ok(value) => serde_json::to_string(&value)?,
        Err(error) => {
            println!("{}", json!({"status":"error","error":error}));
            std::process::exit(1);
        }
    };
    println!("{value}");
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
        [command, db] if command == "migrate-games" => {
            let path = Path::new(db);
            let _lock = lock_database(path)?;
            Database::migrate_games(path)
        }
        [command, db, artifact] if command == "import-lichess-packs" => {
            let path = Path::new(db);
            let _lock = lock_database(path)?;
            let mut database = Database::init(path)?;
            let report = lichess_catalog::import(&mut database, Path::new(artifact))?;
            println!("{}", serde_json::to_string(&report)?);
            Ok(())
        }
        [command, source, dest] if command == "import-openings" => {
            let _lock = lock_database(Path::new(dest))?;
            legacy::import(Path::new(source), Path::new(dest))
        }
        [command, db, kind, input] if command == "import-content" => {
            let _lock = lock_database(Path::new(db))?;
            let mut database = Database::init(Path::new(db))?;
            let input = std::io::BufReader::new(std::fs::File::open(input)?);
            let id = match kind.as_str() {
                "opening" => {
                    let request: SaveOpening = serde_json::from_reader(input)?;
                    let plan = catalog::prepare_opening(&request.opening)?;
                    apply_opening(
                        &mut database,
                        &plan,
                        request.destination.as_ref(),
                        request.source_opening.as_deref(),
                        false,
                    )?
                }
                "gap" => {
                    println!(
                        "{}",
                        gap_edit::import(&mut database, serde_json::from_reader(input)?)?
                    );
                    return Ok(());
                }
                "tactic" => apply_puzzle(
                    &mut database,
                    &prepare_puzzle(serde_json::from_reader(input)?)?,
                )?,
                _ => anyhow::bail!("choose opening, tactic, or gap"),
            };
            println!("{id}");
            Ok(())
        }
        [command] if command == "repertoire-similarity" => {
            print_similarity(repertoire_similarity::cli(io::stdin().lock()))
        }
        [command, db, repertoire, color] if command == "repertoire-similarity-input" => {
            print_similarity(
                repertoire_similarity::export(Path::new(db), repertoire, color).map_err(|error| {
                    error
                        .downcast_ref()
                        .copied()
                        .unwrap_or(repertoire_similarity::Error::Unavailable)
                }),
            )
        }
        [command, db] if command == "opening-rank-input" => {
            let database = Database::read(Path::new(db))?;
            let snapshot = database.unchecked_transaction()?;
            serde_json::to_writer(std::io::stdout().lock(), &opening_rank::read(&snapshot)?)?;
            Ok(())
        }
        [command, db, input] if command == "opening-rank-apply" => {
            let _lock = lock_database(Path::new(db))?;
            let mut database = Database::open(Path::new(db))?;
            let snapshot =
                serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(input)?))?;
            opening_rank::replace(&mut database, snapshot)
        }
        _ => anyhow::bail!(
            "usage: tempo serve NEW_DB [PORT [--no-worker]] | tempo migrate-games DB | tempo import-lichess-packs DB CATALOG | tempo import-openings OLD_DB NEW_DB | tempo opening-rank-input DB | tempo opening-rank-apply DB SNAPSHOT | tempo repertoire-similarity | tempo repertoire-similarity-input DB REPERTOIRE COLOR"
        ),
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn incomplete_pgn_receive_uses_only_its_route_slot() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("fixture.sqlite3");
        let db = Database::init(&database)?;
        let app = Arc::new(Runtime::new(
            database.clone(),
            writer::Writer::start(db, None)?,
            uploads::Staging::start(&database)?,
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let server = tokio::spawn(
            axum::serve(listener, router(Path::new("."), port, app.clone())).into_future(),
        );
        let origin = format!("http://127.0.0.1:{port}/api");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()?;
        let tactic = json!({"name":"Puzzle","pgn":"1. d4 d5 1-0"});
        let upload = json!({"username":"Learner",
            "pgn":"[White \"Learner\"]\n[Black \"Other\"]\n\n1. e4 e5 1-0"});
        let post = |path, body: &Value| client.post(format!("{origin}/{path}")).json(body).send();
        for (path, blocked, other, allowed, slot) in [
            (
                "games/uploads",
                &upload,
                "tactics",
                &tactic,
                &app.games_pgn_receive,
            ),
            (
                "tactics",
                &tactic,
                "games/uploads",
                &upload,
                &app.tactics_pgn_receive,
            ),
        ] {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
            let prefix = format!(
                "POST /api/{path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: 1024\r\n\r\n{{"
            );
            stream.write_all(prefix.as_bytes()).await?;
            tokio::time::timeout(Duration::from_secs(1), async {
                while slot.available_permits() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            let refused = post(path, blocked).await?;
            assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(refused.json::<Value>().await?["code"], "pgn_receive_busy");
            client
                .get(format!("{origin}/health"))
                .send()
                .await?
                .error_for_status()?;
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let response = post(other, allowed).await?;
                    if response.status() != StatusCode::SERVICE_UNAVAILABLE {
                        response.error_for_status()?;
                        return Ok::<_, reqwest::Error>(());
                    }
                    assert_eq!(
                        response.json::<Value>().await?["code"],
                        "staging_unavailable"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await??;
            drop(stream);
            drop(tokio::time::timeout(Duration::from_secs(1), slot.acquire()).await??);
        }
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn preparation_slots_keep_preview_staging_and_commands_independent() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("tempo-preview-{:x}", rand::random::<u128>()));
        std::fs::create_dir(&directory)?;
        let database = directory.join("fixture.sqlite3");
        let mut db = Database::init(&database)?;
        queue_store::seed_batch(&mut db)?;
        let app = Arc::new(Runtime::new(
            database.clone(),
            writer::Writer::start(db, None)?,
            uploads::Staging::start(&database)?,
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let routes = router(Path::new("."), port, app.clone());
        let server = tokio::spawn(async move { axum::serve(listener, routes).await });
        let (started, running) = oneshot::channel();
        let (release, held) = oneshot::channel();
        let caller = tokio::spawn(run_preview(
            OPENING_PREVIEW.try_acquire()?,
            Instant::now() + PGN_DEADLINE,
            move || {
                let _ = started.send(());
                let _ = held.blocking_recv();
                Ok(())
            },
        ));
        running.await?;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());

        let client = reqwest::Client::new();
        let post = |route: &str, data: Value| {
            client
                .post(format!("http://127.0.0.1:{port}/api/{route}"))
                .timeout(Duration::from_secs(3))
                .json(&data)
                .send()
        };
        let preview = json!({"pgn": "1. c4 e5 1-0"});
        let response = post("openings/pgn", preview.clone()).await?;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.json::<Value>().await?["code"], "view_unavailable");

        let staging_slots = app.games_upload_staging.clone().try_acquire_many_owned(2)?;
        let upload_payload = json!({"username":"Learner",
            "pgn":"[White \"Learner\"]\n[Black \"Other\"]\n\n1. e4 e5 1-0"});
        let blocked = post("games/uploads", upload_payload.clone()).await?;
        assert_eq!(blocked.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            blocked.json::<Value>().await?["code"],
            "staging_unavailable"
        );
        let jobs: usize =
            Database::read(&database)?
                .query_row("SELECT count(*) FROM jobs", [], |row| row.get(0))?;
        assert_eq!(jobs, 0);
        for (route, payload) in [
            ("tactics", json!({"name":"Puzzle","pgn":"1. d4 d5 1-0"})),
            (
                "openings",
                json!({"name":"Line","color":"white",
                "route":{"fen":chess::fen(&chess::position("")?),"moves":["e2e4"]}}),
            ),
        ] {
            post(route, payload).await?.error_for_status()?;
        }
        drop(staging_slots);
        let content_slots = app.content_preparation.clone().try_acquire_many_owned(2)?;
        let mut upload = post("games/uploads", upload_payload.clone()).await?;
        if upload.status() == StatusCode::SERVICE_UNAVAILABLE {
            assert_eq!(upload.json::<Value>().await?["code"], "staging_unavailable");
            tokio::time::sleep(Duration::from_millis(100)).await;
            upload = post("games/uploads", upload_payload).await?;
        }
        upload.error_for_status()?;
        let attempt: Value = post("study/next", json!({"after":null}))
            .await?
            .error_for_status()?
            .json()
            .await?;
        let result: Value = post(
            "study/answer",
            json!({
                "attempt":attempt["attempt"], "dataset":attempt["dataset"], "played":null,
            }),
        )
        .await?
        .error_for_status()?
        .json()
        .await?;
        assert_eq!(result["rating"], "Again");
        drop(content_slots);

        release.send(()).unwrap();
        drop(tokio::time::timeout(Duration::from_secs(1), OPENING_PREVIEW.acquire()).await??);
        let route: Value = post("openings/pgn", preview)
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(route["moves"], json!(["c2c4", "e7e5"]));
        server.abort();
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn expired_preview_input_stops_the_pgn_reader() {
        let error = chess::preview_pgn(PreviewInput {
            bytes: b"1. c4 e5 1-0",
            deadline: Instant::now(),
        })
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::TimedOut
        );
    }
}
