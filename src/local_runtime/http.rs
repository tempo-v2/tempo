//! Serve public UI artifacts and serialize commands through a single Tempo app.
mod ingress;

use self::ingress::{Admission, Head, Header, MAX_REPLY_BYTES, Request, Response, Server};
use super::{LocalResult, tactic_settings};
use crate::{
    AppError, SaveStudyReview, SavedStudyReviewCommand, Tempo,
    game_backfill::{BackfillCoordinator, BackfillRequest, BackfillRetry},
    game_evidence::{GameImport, GameJobs, GameRefresh},
    game_model::{AuthoringSurface, GameModel, GapRequest, ModelRequest},
    gap_explorer::{GapExplorer, GapExplorerRequest},
    wire::{ErrorView, RepertoireCommand, StudyCommand, StudyView, TacticCaptureCommand},
};
use hyper::{
    Method, StatusCode,
    body::Bytes,
    header::{HeaderName, HeaderValue},
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tempo_store::SqliteStore;

const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_CONTENT_COMMAND_BYTES: usize = 2 * 1024 * 1024;

pub(super) fn serve(
    app: &mut Tempo,
    profile_store: &SqliteStore,
    database: &Path,
    ui: &Path,
    port: u16,
) -> LocalResult<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    let address = listener.local_addr()?.to_string();
    let surfaces = Arc::new(HttpSurfaces {
        origin: format!("http://{address}"),
        address,
        desktop: build_files(ui)?,
        phone: PhoneStudy::load(ui)?,
    });
    let ingress_surfaces = surfaces.clone();
    let server = Server::start(listener, move |head| ingress_surfaces.admitted(head))?;
    println!("Tempo study at {}", surfaces.origin);
    crate::diagnostics::emit(
        "runtime.listening",
        serde_json::json!({
            "listen_port": surfaces.address.rsplit(':').next().and_then(|port| port.parse::<u16>().ok())
        }),
    );
    let explorer = std::sync::Arc::new(crate::explorer::ExplorerClient::new(database));
    let mut games = Games {
        refresh: GameJobs::start()?,
        backfill: BackfillCoordinator::start(app)?,
        model: GameModel::new(database)?,
        coverage: crate::coverage_scan::CoverageScan::new(database, explorer.clone()),
        gap_explorer: GapExplorer::with_client(explorer),
    };
    let mut model_marker = None;
    let mut next_model_check = Instant::now();
    loop {
        if let Some(request) = server.try_recv()? {
            if let Err(error) = respond(app, profile_store, &mut games, request, &surfaces) {
                eprintln!("Tempo HTTP: {error}");
            }
            continue;
        }
        if Instant::now() >= next_model_check {
            let mut trace = crate::diagnostics::Span::new("loop.model_schedule");
            let scheduled = (|| -> LocalResult<()> {
                let marker = profile_store.game_comparison_marker()?;
                if model_marker.as_ref() != Some(&marker) {
                    games.model.project_cached(marker.0.clone())?;
                    model_marker = Some(marker);
                }
                Ok(())
            })();
            if let Err(error) = scheduled {
                eprintln!("Tempo model scheduling: {error}");
                trace.set_outcome("error");
                crate::diagnostics::emit(
                    "runtime.error",
                    serde_json::json!({
                        "operation": "loop.model_schedule", "error_code": "model_scheduling_failed"
                    }),
                );
            }
            next_model_check = Instant::now() + Duration::from_millis(250);
        }
        {
            let _trace = crate::diagnostics::Span::new("loop.refresh_apply");
            games.refresh.poll(app);
        }
        if !games.refresh.active() {
            let _trace = crate::diagnostics::Span::new("loop.backfill_apply");
            games.backfill.poll(app);
        }
        let Some(request) = server.recv_timeout(Duration::from_millis(250))? else {
            continue;
        };
        if let Err(error) = respond(app, profile_store, &mut games, request, &surfaces) {
            eprintln!("Tempo HTTP: {error}");
            crate::diagnostics::emit(
                "runtime.error",
                serde_json::json!({
                    "operation": "http.request", "error_code": "http_io_error"
                }),
            );
        }
    }
}

struct Games {
    refresh: GameJobs,
    backfill: BackfillCoordinator,
    model: GameModel,
    coverage: crate::coverage_scan::CoverageScan,
    gap_explorer: GapExplorer,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GapPage {
    offset: usize,
    forecast_repertoire: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StudyAttemptRead {
    dataset_id: tempo_types::DatasetId,
    attempt_id: tempo_types::AttemptId,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StudyAnalysisRead {
    dataset_id: tempo_types::DatasetId,
    attempt_id: tempo_types::AttemptId,
    revision: tempo_types::Revision,
    content_version: tempo_types::ContentVersion,
}

struct HttpSurfaces {
    address: String,
    origin: String,
    desktop: BTreeMap<String, Asset>,
    phone: Option<PhoneStudy>,
}

impl HttpSurfaces {
    fn admitted(&self, head: &Head) -> Result<Admission, StatusCode> {
        // Check the forwarded surface first, even when phone access is disabled.
        let surface = if head.headers().contains_key("X-Forwarded-Host")
            || head.headers().contains_key("X-Forwarded-Proto")
        {
            let phone = self
                .phone
                .as_ref()
                .filter(|phone| phone.authorizes(head))
                .ok_or(StatusCode::FORBIDDEN)?;
            if !phone.exposes(head) {
                return Err(StatusCode::NOT_FOUND);
            }
            AuthoringSurface::PhoneBuild
        } else {
            if head.header("Host") != Some(self.address.as_str())
                || (head.headers().contains_key("Origin")
                    && head.header("Origin") != Some(self.origin.as_str()))
            {
                return Err(StatusCode::FORBIDDEN);
            }
            AuthoringSurface::Desktop
        };
        let body_bytes = (head.method() == Method::POST).then(|| match head.url() {
            "/api/study/presence" => 1024,
            "/api/repertoires" | "/api/tactics" | "/api/games/import" => MAX_CONTENT_COMMAND_BYTES,
            _ => MAX_COMMAND_BYTES,
        });
        Ok(Admission {
            surface,
            body_bytes,
        })
    }

    fn assets(&self, surface: AuthoringSurface) -> &BTreeMap<String, Asset> {
        match surface {
            AuthoringSurface::Desktop => &self.desktop,
            AuthoringSurface::PhoneBuild => {
                &self.phone.as_ref().expect("admitted phone surface").assets
            }
        }
    }
}

struct PhoneStudy {
    origin: String,
    authority: String,
    login: String,
    assets: BTreeMap<String, Asset>,
}

impl PhoneStudy {
    fn load(ui: &Path) -> LocalResult<Option<Self>> {
        use std::env::VarError::NotPresent;

        let (origin, login) = match (
            std::env::var("TEMPO_PHONE_ORIGIN"),
            std::env::var("TEMPO_PHONE_TAILSCALE_LOGIN"),
        ) {
            (Err(NotPresent), Err(NotPresent)) => return Ok(None),
            (Ok(origin), Ok(login)) if !login.trim().is_empty() => (origin, login),
            _ => return Err("set both Tempo phone origin and Tailscale login".into()),
        };
        let authority = origin
            .strip_prefix("https://")
            .filter(|authority| valid_phone_authority(authority))
            .ok_or("expected a Tempo phone HTTPS origin with a .ts.net host and explicit port")?
            .to_owned();
        Ok(Some(Self {
            origin,
            authority,
            login,
            assets: build_files(&ui.join("phone"))?,
        }))
    }

    fn authorizes(&self, request: &Head) -> bool {
        let matches = |name, expected: &str| {
            let values = request.headers().get_all(name);
            let mut values = values.iter();
            values.next().and_then(|value| value.to_str().ok()) == Some(expected)
                && values.next().is_none()
        };
        matches("Host", &self.authority)
            && matches("X-Forwarded-Host", &self.authority)
            && matches("X-Forwarded-Proto", "https")
            && matches("Tailscale-User-Login", &self.login)
            && !request.headers().contains_key("Tailscale-Funnel-Request")
            && (matches("Origin", &self.origin)
                || (request.method() == &Method::GET && !request.headers().contains_key("Origin")))
    }

    fn exposes(&self, request: &Head) -> bool {
        match request.method() {
            &Method::GET => {
                request.url() == "/api/context"
                    || self.assets.contains_key(asset_path(request.url()))
            }
            &Method::POST => matches!(
                request.url(),
                "/api/study"
                    | "/api/study/presence"
                    | "/api/study-progress"
                    | "/api/study/relevance"
                    | "/api/study/game-context"
                    | "/api/study/review-mark"
                    | "/api/study/card-details"
                    | "/api/repertoires"
                    | "/api/games/coverage"
                    | "/api/games/coverage/status"
                    | "/api/games/coverage/starting-line"
                    | "/api/games/coverage/starting-line/save"
                    | "/api/games/gaps"
                    | "/api/games/gap-draft"
                    | "/api/games/gap-line"
                    | "/api/games/gap-line/status"
                    | "/api/games/gap-analysis"
                    | "/api/games/gap-analysis/status"
            ),
            _ => false,
        }
    }
}

fn valid_phone_authority(authority: &str) -> bool {
    let Some((host, port)) = authority.rsplit_once(':') else {
        return false;
    };
    host.ends_with(".ts.net")
        && host.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        && port
            .parse::<u16>()
            .is_ok_and(|port| port > 0 && port != 443)
}

fn asset_path(url: &str) -> &str {
    url.split('?').next().unwrap_or_default()
}

struct Asset {
    mime: &'static str,
    bytes: Bytes,
}

fn read_asset(path: &Path) -> LocalResult<Bytes> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((MAX_REPLY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_REPLY_BYTES {
        return Err("Tempo UI asset exceeds the response budget".into());
    }
    Ok(bytes.into())
}

fn build_files(ui: &Path) -> LocalResult<BTreeMap<String, Asset>> {
    let index = ui.join("index.html");
    let directory = ui.join("assets");
    if !fs::symlink_metadata(&index)?.is_file() || !fs::symlink_metadata(&directory)?.is_dir() {
        return Err("expected a built Tempo UI with index.html and assets/".into());
    }
    let mut files = BTreeMap::from([(
        "/".to_owned(),
        Asset {
            mime: "text/html; charset=utf-8",
            bytes: read_asset(&index)?,
        },
    )]);
    for path in fs::read_dir(directory)? {
        let path = path?;
        if !path.file_type()?.is_file() {
            continue;
        }
        let filename = path.file_name();
        let Some(name) = filename.to_str() else {
            continue;
        };
        let (base, extension) = name.rsplit_once('.').unwrap_or_default();
        if base.is_empty()
            || !base
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            continue;
        }
        let mime = match extension {
            "js" => "text/javascript; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "svg" => "image/svg+xml",
            "png" => "image/png",
            "webmanifest" => "application/manifest+json",
            "mp3" => "audio/mpeg",
            "woff2" => "font/woff2",
            _ => continue,
        };
        files.insert(
            format!("/assets/{name}"),
            Asset {
                mime,
                bytes: read_asset(&path.path())?,
            },
        );
    }
    Ok(files)
}

fn respond(
    app: &mut Tempo,
    profile_store: &SqliteStore,
    games: &mut Games,
    mut request: Request,
    surfaces: &HttpSurfaces,
) -> LocalResult<()> {
    if !request.claim() {
        request.busy();
        return Ok(());
    }
    let context = crate::diagnostics::RequestContext::new(Some(request.delivery_id()));
    let mut trace = crate::diagnostics::Span::new("http.request");
    trace.field("request_id", context.id());
    trace.field("queue_wait_ms", request.queue_wait().as_millis() as u64);
    trace.field(
        "method",
        match request.method() {
            &Method::GET => "GET",
            &Method::POST => "POST",
            &Method::HEAD => "HEAD",
            &Method::PUT => "PUT",
            &Method::DELETE => "DELETE",
            &Method::OPTIONS => "OPTIONS",
            &Method::PATCH => "PATCH",
            _ => "OTHER",
        },
    );
    trace.field("route", diagnostic_route(request.url()));
    let result = respond_inner(app, profile_store, games, request, surfaces);
    trace.set_outcome(if result.is_ok() {
        "returned"
    } else {
        "io_error"
    });
    result
}

fn respond_inner(
    app: &mut Tempo,
    profile_store: &SqliteStore,
    games: &mut Games,
    request: Request,
    surfaces: &HttpSurfaces,
) -> LocalResult<()> {
    let surface = request.surface();
    let public = surfaces.assets(surface);
    if request.method() == &Method::GET && request.url() == "/api/context" {
        let (status, reply) = match profile_store.context_identity() {
            Ok((dataset, schema_version)) => (
                200,
                serde_json::json!({
                    "status": "ready",
                    "dataset_id": dataset,
                    "schema_version": schema_version,
                }),
            ),
            Err(_) => (
                503,
                serde_json::json!({"status": "unavailable", "code": "storage_unavailable"}),
            ),
        };
        respond_traced(
            request,
            body("application/json; charset=utf-8", json_bytes(&reply)?)
                .with_status_code(status_code(status))
                .with_header(field("Cache-Control", "no-store")),
        )?;
        return Ok(());
    }
    // Older open tabs have strict v5 reply validators. The opt-in changes
    // display only, independent of submission identity and saved receipts.
    let move_details = header(&request, "X-Tempo-Move-Details") == Some("1");
    let opponent_context = header(&request, "X-Tempo-Study-Opponent-Moves") == Some("1");
    let tactic_explanation = header(&request, "X-Tempo-Study-Tactic-Explanation") == Some("1");
    if request.method() == &Method::POST
        && matches!(
            request.url(),
            "/api/study/presence"
                | "/api/games/tactics"
                | "/api/games/tactics/command"
                | "/api/games/tactics/report"
                | "/api/games/tactics/position"
        )
    {
        let retired = serde_json::json!({
            "status": "error", "code": "retired_feature",
            "message": "Automatic tactical analysis and Study presence have been retired.",
            "retryable": false,
        });
        respond_traced(
            request,
            body("application/json; charset=utf-8", json_bytes(&retired)?)
                .with_status_code(StatusCode::GONE),
        )?;
    } else if matches!(
        (request.method(), request.url()),
        (&Method::GET, "/api/games/detectors")
            | (
                &Method::POST,
                "/api/games/detection" | "/api/games/detection-result"
            )
    ) {
        let retired = serde_json::json!({
            "status": "error", "code": "retired_feature",
            "message": "The manual game detector has been retired.", "retryable": false,
        });
        respond_traced(
            request,
            body("application/json; charset=utf-8", json_bytes(&retired)?)
                .with_status_code(StatusCode::GONE),
        )?;
    } else if request.method() == &Method::POST
        && matches!(
            request.url(),
            "/api/analysis/evidence" | "/api/analysis/result"
        )
    {
        let retired = serde_json::json!({
            "status": "error", "code": "retired_feature",
            "message": "Position evidence has been retired.", "retryable": false,
        });
        respond_traced(
            request,
            body("application/json; charset=utf-8", json_bytes(&retired)?)
                .with_status_code(StatusCode::GONE),
        )?;
    } else if matches!(
        (request.method(), request.url()),
        (&Method::GET | &Method::POST, "/api/games/gap-analysis")
            | (&Method::POST, "/api/games/gap-analysis/status")
    ) {
        let retired = serde_json::json!({
            "status": "error", "code": "retired_feature",
            "message": "Standalone gap analysis has been retired.", "retryable": false,
        });
        respond_traced(
            request,
            body("application/json; charset=utf-8", json_bytes(&retired)?)
                .with_status_code(StatusCode::GONE),
        )?;
    } else if request.method() == &Method::POST
        && matches!(
            request.url(),
            "/api/games/gap-line" | "/api/games/gap-line/status"
        )
    {
        let retired = serde_json::json!({
            "status": "error", "code": "retired_feature",
            "message": "Automatic gap-line previews have been retired.", "retryable": false,
        });
        respond_traced(
            request,
            body("application/json; charset=utf-8", json_bytes(&retired)?)
                .with_status_code(StatusCode::GONE),
        )?;
    } else if matches!(
        request.url(),
        "/api/games"
            | "/api/games/library"
            | "/api/games/job"
            | "/api/games/import"
            | "/api/games/refresh"
            | "/api/games/choose"
            | "/api/games/reprocess"
            | "/api/games/backfill"
            | "/api/games/backfill/retry"
            | "/api/games/model"
            | "/api/games/coverage"
            | "/api/games/coverage/status"
            | "/api/games/coverage/starting-line"
            | "/api/games/coverage/starting-line/save"
            | "/api/games/gaps"
            | "/api/games/gap-draft"
            | "/api/games/review"
            | "/api/games/gap-explorer"
            | "/api/study/relevance"
            | "/api/study/game-context"
            | "/api/study/analysis-context"
            | "/api/study/analysis-line"
            | "/api/study/card-details"
    ) {
        let (status, reply) = match (request.method(), request.url()) {
            (&Method::POST, "/api/games/library") => (
                200,
                command::<crate::GameLibraryRead>(&request, MAX_COMMAND_BYTES)
                    .and_then(|r| app.game_library(r)),
            ),
            (&Method::GET, "/api/games") => (200, app.game_history()),
            (&Method::GET, "/api/games/job") => (200, Ok(games.refresh.poll(app))),
            (&Method::GET, "/api/games/backfill") => (200, games.backfill.current_status(app)),
            (&Method::POST, "/api/games/backfill") => (
                202,
                command::<BackfillRequest>(&request, MAX_COMMAND_BYTES)
                    .and_then(|intent| games.backfill.begin(app, intent)),
            ),
            (&Method::POST, "/api/games/backfill/retry") => (
                202,
                command::<BackfillRetry>(&request, MAX_COMMAND_BYTES)
                    .and_then(|intent| games.backfill.retry(app, intent)),
            ),
            (&Method::GET, "/api/games/model") => (200, games.model.status(app)),
            (&Method::POST, "/api/games/model") => (
                202,
                command::<ModelRequest>(&request, MAX_COMMAND_BYTES)
                    .and_then(|intent| games.model.start(app, intent)),
            ),
            (&Method::POST, "/api/games/coverage") => (
                202,
                command::<crate::coverage_scan::StartCoverage>(&request, MAX_COMMAND_BYTES)
                    .and_then(|intent| games.coverage.start(app, intent)),
            ),
            (&Method::POST, "/api/games/coverage/status") => (
                200,
                command::<crate::coverage_scan::ReadCoverage>(&request, MAX_COMMAND_BYTES)
                    .and_then(|intent| games.coverage.read(app, intent)),
            ),
            (&Method::POST, "/api/games/coverage/starting-line") => (
                200,
                command::<crate::application::PreviousCoverageLineRequest>(
                    &request,
                    MAX_COMMAND_BYTES,
                )
                .and_then(|intent| match intent {
                    crate::application::PreviousCoverageLineRequest::Read(read) => {
                        app.coverage_line_options(read)
                    }
                    crate::application::PreviousCoverageLineRequest::Save(save) => {
                        app.save_coverage_line(save)
                    }
                })
                .map(|reply| serde_json::json!(reply)),
            ),
            (&Method::POST, "/api/games/coverage/starting-line/save") => (
                200,
                command::<crate::application::CoverageLineSave>(&request, MAX_COMMAND_BYTES)
                    .and_then(|intent| app.save_coverage_line(intent))
                    .map(|reply| serde_json::json!(reply)),
            ),
            (&Method::GET, "/api/games/gaps") => (200, app.opponent_gaps(0)),
            (&Method::POST, "/api/games/gaps") => (
                200,
                command::<GapPage>(&request, MAX_COMMAND_BYTES).and_then(|page| {
                    if surface == AuthoringSurface::PhoneBuild
                        && page
                            .forecast_repertoire
                            .as_deref()
                            .is_none_or(str::is_empty)
                    {
                        return Err(AppError::new(
                            "phone_build_only",
                            "Choose a repertoire to view its coverage gaps.",
                        ));
                    }
                    app.opponent_gap_page(page.offset, page.forecast_repertoire.as_deref())
                }),
            ),
            (&Method::GET, "/api/games/gap-explorer") => (200, Ok(games.gap_explorer.status())),
            (&Method::POST, "/api/games/gap-explorer") => (
                202,
                command::<GapExplorerRequest>(&request, MAX_COMMAND_BYTES)
                    .and_then(|intent| games.gap_explorer.start(app, intent)),
            ),
            (&Method::POST, "/api/games/review") => (
                200,
                command::<crate::GameReviewRead>(&request, MAX_COMMAND_BYTES)
                    .and_then(|request| app.game_review(request)),
            ),
            (&Method::POST, "/api/games/gap-draft") => (
                200,
                command::<GapRequest>(&request, MAX_COMMAND_BYTES)
                    .and_then(|intent| app.game_gap_draft_on(intent, surface)),
            ),
            (&Method::POST, "/api/study/relevance") => (
                200,
                command::<StudyAttemptRead>(&request, MAX_COMMAND_BYTES).and_then(|intent| {
                    app.study_relevance_reason(&intent.dataset_id, &intent.attempt_id)
                }),
            ),
            (&Method::POST, "/api/study/analysis-context") => (
                200,
                command::<StudyAnalysisRead>(&request, MAX_COMMAND_BYTES).and_then(|intent| {
                    app.study_analysis_context(
                        &intent.dataset_id,
                        &intent.attempt_id,
                        intent.revision,
                        intent.content_version,
                    )
                }),
            ),
            (&Method::POST, "/api/study/analysis-line") => (
                200,
                command::<StudyAnalysisRead>(&request, MAX_COMMAND_BYTES).and_then(|intent| {
                    app.study_analysis_line(
                        &intent.dataset_id,
                        &intent.attempt_id,
                        intent.revision,
                        intent.content_version,
                    )
                    .map(|line| serde_json::json!(line))
                }),
            ),
            (&Method::POST, "/api/study/card-details") => (
                200,
                command::<StudyAnalysisRead>(&request, MAX_COMMAND_BYTES).and_then(|intent| {
                    app.study_card_details(
                        &intent.dataset_id,
                        &intent.attempt_id,
                        intent.revision,
                        intent.content_version,
                    )
                }),
            ),
            (&Method::POST, "/api/study/game-context") => (
                200,
                command::<StudyAttemptRead>(&request, MAX_COMMAND_BYTES).and_then(|intent| {
                    let mut reply =
                        app.study_game_context(&intent.dataset_id, &intent.attempt_id)?;
                    if !move_details && let Some(context) = reply["context"].as_object_mut() {
                        context.remove("played_uci");
                    }
                    Ok(reply)
                }),
            ),
            (&Method::POST, "/api/games/import") => (
                200,
                command::<GameImport>(&request, MAX_CONTENT_COMMAND_BYTES)
                    .and_then(|intent| app.import_game_pgn(intent))
                    .map(|report| serde_json::json!(report)),
            ),
            (&Method::POST, "/api/games/choose") => (
                200,
                command::<crate::game_evidence::ChooseGameCorrection>(&request, MAX_COMMAND_BYTES)
                    .and_then(|intent| app.choose_game_correction(intent))
                    .map(|report| serde_json::json!(report)),
            ),
            (&Method::POST, "/api/games/reprocess") => (
                200,
                command::<crate::game_evidence::ReprocessGames>(&request, MAX_COMMAND_BYTES)
                    .and_then(|intent| app.reprocess_games(intent))
                    .map(|report| serde_json::json!(report)),
            ),
            (&Method::POST, "/api/games/refresh") => (
                202,
                command::<GameRefresh>(&request, MAX_COMMAND_BYTES)
                    .and_then(|intent| games.refresh.refresh(app, intent, !games.backfill.active()))
                    .map(|reply| serde_json::json!(reply)),
            ),
            _ => (
                405,
                Err(AppError::new(
                    "method_not_allowed",
                    "Invalid Games request.",
                )),
            ),
        };
        let (status, reply) = match reply {
            Ok(reply) => (status, reply),
            Err(error) => (
                if status == 405 { 405 } else { 400 },
                serde_json::json!(ErrorView::for_content(error)),
            ),
        };
        respond_traced(
            request,
            body("application/json; charset=utf-8", json_bytes(&reply)?)
                .with_status_code(status_code(status))
                .with_header(field("Cache-Control", "no-store")),
        )?;
    } else if request.method() == &Method::POST && request.url() == "/api/tactic-settings" {
        let reply = command(&request, MAX_COMMAND_BYTES)
            .and_then(|intent| tactic_settings::handle(app, profile_store, intent))
            .unwrap_or_else(|error| serde_json::json!(ErrorView::for_content(error)));
        respond_traced(
            request,
            body("application/json; charset=utf-8", json_bytes(&reply)?),
        )?;
    } else if request.method() == &Method::POST && request.url() == "/api/tactics-collection" {
        let reply = command::<crate::TacticsRead>(&request, MAX_COMMAND_BYTES)
            .and_then(|intent| app.read_tactics(intent))
            .map(|content| serde_json::json!(content))
            .unwrap_or_else(|error| serde_json::json!(StudyView::from(error)));
        respond_traced(
            request,
            body("application/json; charset=utf-8", json_bytes(&reply)?),
        )?;
    } else if request.method() == &Method::POST
        && matches!(
            request.url(),
            "/api/study/review-mark" | "/api/study/saved-reviews"
        )
    {
        let reply = match request.url() {
            "/api/study/review-mark" => command::<SaveStudyReview>(&request, MAX_COMMAND_BYTES)
                .and_then(|intent| app.save_study_review(intent)),
            _ => command::<SavedStudyReviewCommand>(&request, MAX_COMMAND_BYTES)
                .and_then(|intent| app.saved_study_reviews(intent)),
        }
        .unwrap_or_else(|error| serde_json::json!(StudyView::from(error)));
        respond_traced(
            request,
            body("application/json; charset=utf-8", json_bytes(&reply)?)
                .with_header(field("Cache-Control", "no-store")),
        )?;
    } else if request.method() == &Method::POST && request.url() == "/api/study-progress" {
        let reply = command::<Option<crate::ProgressStamp>>(&request, MAX_COMMAND_BYTES)
            .and_then(|known| app.study_progress(known.as_ref()))
            .map(|progress| serde_json::json!(progress))
            .unwrap_or_else(|error| serde_json::json!(StudyView::from(error)));
        respond_traced(
            request,
            body("application/json; charset=utf-8", json_bytes(&reply)?),
        )?;
    } else if request.method() == &Method::POST && request.url() == "/api/study" {
        let reply = command::<StudyCommand>(&request, MAX_COMMAND_BYTES)
            .map(|intent| app.handle(intent))
            .unwrap_or_else(Into::into);
        let reply = if move_details {
            reply
        } else {
            reply.without_move_details()
        };
        let reply = if opponent_context {
            reply
        } else {
            reply.without_opponent_context()
        };
        let reply = if tactic_explanation {
            reply
        } else {
            reply.without_tactic_explanation()
        };
        respond_traced(
            request,
            body("application/json; charset=utf-8", json_bytes(&reply)?),
        )?;
    } else if request.method() == &Method::POST
        && matches!(request.url(), "/api/repertoires" | "/api/tactics")
    {
        let reply = if request.url() == "/api/repertoires" {
            content_command(&request, |intent: RepertoireCommand| {
                app.handle_repertoire_on(intent, surface)
            })?
        } else {
            content_command(&request, |intent: TacticCaptureCommand| {
                app.handle_tactic_capture(intent)
            })?
        };
        respond_traced(
            request,
            body("application/json; charset=utf-8", json_bytes(&reply)?),
        )?;
    } else if request.method() == &Method::GET {
        if let Some(asset) = public.get(asset_path(request.url())) {
            respond_traced(request, body(asset.mime, asset.bytes.clone()).with_header(
                field("Content-Security-Policy", "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; object-src 'none'; frame-ancestors 'none'"),
            ))?;
        } else {
            respond_traced(request, Response::empty(StatusCode::NOT_FOUND))?;
        }
    } else {
        respond_traced(request, Response::empty(StatusCode::METHOD_NOT_ALLOWED))?;
    }
    Ok(())
}

fn content_command<T: DeserializeOwned>(
    request: &Request,
    handle: impl FnOnce(T) -> Value,
) -> LocalResult<Value> {
    Ok(match command(request, MAX_CONTENT_COMMAND_BYTES) {
        Ok(intent) => handle(intent),
        Err(error) => serde_json::to_value(ErrorView::for_content(error))?,
    })
}

fn command<T: DeserializeOwned>(request: &Request, max_bytes: usize) -> Result<T, AppError> {
    let invalid = |message| AppError::new("invalid_command", message);
    let mime = header(request, "Content-Type").and_then(|value| value.split(';').next());
    if mime != Some("application/json") || request.body().len() > max_bytes {
        return Err(invalid(
            "Use an application/json command within the request limit.",
        ));
    }
    let mut decode_trace = crate::diagnostics::Span::new("http.decode_body");
    let result =
        serde_json::from_slice(request.body()).map_err(|_| invalid("The command is invalid."));
    if result.is_err() {
        decode_trace.set_outcome("invalid_json");
    }
    result
}

fn header<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request.header(name)
}

fn field(name: &str, value: &str) -> Header {
    (
        HeaderName::from_bytes(name.as_bytes()).expect("HTTP header name"),
        HeaderValue::from_str(value).expect("HTTP header value"),
    )
}

fn status_code(status: u16) -> StatusCode {
    StatusCode::from_u16(status).expect("valid HTTP status")
}

fn json_bytes(value: &impl serde::Serialize) -> serde_json::Result<Vec<u8>> {
    struct BoundedResponse(Vec<u8>);
    impl Write for BoundedResponse {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_REPLY_BYTES - self.0.len() {
                return Err(std::io::Error::other(
                    "Tempo response exceeds the byte budget",
                ));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut output = BoundedResponse(Vec::new());
    serde_json::to_writer(&mut output, value)?;
    Ok(output.0)
}

fn body(mime: &str, bytes: impl Into<Bytes>) -> Response {
    Response::from_data(bytes)
        .with_header(field("Content-Type", mime))
        .with_header(field("Cache-Control", "no-store"))
        .with_header(field("X-Content-Type-Options", "nosniff"))
}

/// Diagnostics only use known route templates; URLs and query strings are never recorded.
fn diagnostic_route(url: &str) -> &'static str {
    match url.split('?').next().unwrap_or_default() {
        "/api/context" => "context",
        "/api/study" => "study",
        "/api/study-progress" => "study_progress",
        "/api/study/relevance" => "study_relevance",
        "/api/study/game-context" => "study_game_context",
        "/api/study/analysis-context" => "study_analysis_context",
        "/api/study/analysis-line" => "study_analysis_line",
        "/api/study/review-mark" => "study_review_mark",
        "/api/study/card-details" => "study_card_details",
        "/api/study/saved-reviews" => "study_saved_reviews",
        "/api/repertoires" => "repertoires",
        "/api/tactics" => "tactics",
        "/api/tactics-collection" => "tactics_collection",
        "/api/tactic-settings" => "tactic_settings",
        "/api/games" => "games",
        "/api/games/library" => "game_library",
        "/api/games/job" => "game_refresh_status",
        "/api/games/import" => "game_import",
        "/api/games/refresh" => "game_refresh",
        "/api/games/choose" => "game_choose",
        "/api/games/reprocess" => "game_reprocess",
        "/api/games/backfill" => "game_backfill",
        "/api/games/backfill/retry" => "game_backfill_retry",
        "/api/games/model" => "game_model",
        "/api/games/coverage" => "repertoire_coverage_start",
        "/api/games/coverage/starting-line/save" => "save_repertoire_coverage_starting_line",
        "/api/games/coverage/starting-line" => "repertoire_coverage_starting_line",
        "/api/games/coverage/status" => "repertoire_coverage_status",
        "/api/games/gaps" => "game_gaps",
        "/api/games/gap-draft" => "game_gap_draft",
        "/api/games/review" => "game_review",
        "/api/games/gap-explorer" => "game_gap_explorer",
        "/" | "/index.html" => "ui_index",
        path if path.starts_with("/assets/") => "ui_asset",
        _ => "other",
    }
}

/// Queue the detached response; this does not establish network delivery.
fn respond_traced(request: Request, response: Response) -> std::io::Result<()> {
    let mut trace = crate::diagnostics::Span::new("http.write_response");
    let status = response.status_code().as_u16();
    trace.field("status_code", status);
    let outcome = if request.send(response) {
        "queued_for_delivery"
    } else {
        "disconnected"
    };
    trace.set_outcome(outcome);
    crate::diagnostics::emit(
        "http.response",
        serde_json::json!({
            "operation":"http.write_response", "status_code":status, "outcome":outcome
        }),
    );
    Ok(())
}

#[cfg(test)]
mod diagnostics_tests {
    use super::diagnostic_route;

    #[test]
    fn routes_never_include_payloads_or_query_values() {
        assert_eq!(diagnostic_route("/api/study?token=secret"), "study");
        assert_eq!(diagnostic_route("/api/private-player-name"), "other");
        assert_eq!(
            diagnostic_route("/assets/private-build-name.js"),
            "ui_asset"
        );
        assert_eq!(diagnostic_route("https://example.com/api/study"), "other");
    }
}
