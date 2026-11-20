//! Serve public UI artifacts and serialize commands through a single Tempo app.
use super::{LocalResult, tactic_settings};
use crate::{
    AppError, SaveStudyReview, SavedStudyReviewCommand, Tempo,
    game_backfill::{BackfillCoordinator, BackfillRequest, BackfillRetry},
    game_detection::{DetectionRead, DetectionReply, DetectionRequest, GameDetection},
    game_evidence::{GameImport, GameJobs, GameRefresh},
    game_model::{GameModel, GapRequest, ModelRequest},
    gap_analysis::{GapAnalysis, GapAnalysisRequest},
    gap_explorer::{GapExplorer, GapExplorerRequest},
    wire::{ErrorView, RepertoireCommand, StudyCommand, StudyView, TacticCaptureCommand},
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    net::Ipv4Addr,
    path::Path,
    time::{Duration, Instant},
};
use tempo_store::SqliteStore;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const MAX_COMMAND_BYTES: usize = 16 * 1024;

pub(super) fn serve(
    app: &mut Tempo,
    profile_store: &SqliteStore,
    database: &Path,
    ui: &Path,
    port: u16,
    detection: GameDetection,
) -> LocalResult<()> {
    let server = Server::http((Ipv4Addr::LOCALHOST, port))?;
    let address = server.server_addr().to_string();
    let surfaces = HttpSurfaces {
        origin: format!("http://{address}"),
        address,
        desktop: build_files(ui)?,
        phone: PhoneStudy::load(ui)?,
    };
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
        detection,
        backfill: BackfillCoordinator::start(app)?,
        model: GameModel::new(database)?,
        gap_analysis: GapAnalysis::new(database),
        position_evidence: crate::position_evidence::PositionEvidence::new(
            database,
            explorer.clone(),
        ),
        gap_explorer: GapExplorer::with_client(explorer),
    };
    let mut model_marker = None;
    let mut next_model_check = Instant::now();
    loop {
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
        {
            let _trace = crate::diagnostics::Span::new("loop.detection_apply");
            games.detection.poll(app);
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
    detection: GameDetection,
    backfill: BackfillCoordinator,
    model: GameModel,
    gap_analysis: GapAnalysis,
    gap_explorer: GapExplorer,
    position_evidence: crate::position_evidence::PositionEvidence,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GapPage {
    offset: usize,
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
    fn assets_for(&self, request: &Request) -> Result<&BTreeMap<String, Asset>, StatusCode> {
        // Tailscale Serve rewrites the forwarding headers and always marks proxied
        // requests. Classify those before checking the caller-controlled Host,
        // including when phone access is disabled.
        if header(request, "X-Forwarded-Host").is_some()
            || header(request, "X-Forwarded-Proto").is_some()
        {
            let phone = self
                .phone
                .as_ref()
                .filter(|phone| phone.authorizes(request))
                .ok_or(StatusCode(403))?;
            if !phone.exposes(request) {
                return Err(StatusCode(404));
            }
            return Ok(&phone.assets);
        }
        if header(request, "Host") != Some(self.address.as_str())
            || header(request, "Origin").is_some_and(|origin| origin != self.origin)
        {
            return Err(StatusCode(403));
        }
        Ok(&self.desktop)
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

    fn authorizes(&self, request: &Request) -> bool {
        let matches = |name, expected: &str| {
            let mut values = request
                .headers()
                .iter()
                .filter(|field| field.field.equiv(name))
                .map(|field| field.value.as_str());
            values.next() == Some(expected) && values.next().is_none()
        };
        matches("Host", &self.authority)
            && matches("X-Forwarded-Host", &self.authority)
            && matches("X-Forwarded-Proto", "https")
            && matches("Tailscale-User-Login", &self.login)
            && header(request, "Tailscale-Funnel-Request").is_none()
            && (matches("Origin", &self.origin)
                || (request.method() == &Method::Get && header(request, "Origin").is_none()))
    }

    fn exposes(&self, request: &Request) -> bool {
        match request.method() {
            Method::Get => self.assets.contains_key(asset_path(request)),
            Method::Post => matches!(
                request.url(),
                "/api/study"
                    | "/api/study-progress"
                    | "/api/study/relevance"
                    | "/api/study/game-context"
                    | "/api/study/review-mark"
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

fn asset_path(request: &Request) -> &str {
    request.url().split('?').next().unwrap_or_default()
}

struct Asset {
    mime: &'static str,
    bytes: Vec<u8>,
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
            bytes: fs::read(index)?,
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
                bytes: fs::read(path.path())?,
            },
        );
    }
    Ok(files)
}

fn respond(
    app: &mut Tempo,
    profile_store: &SqliteStore,
    games: &mut Games,
    request: Request,
    surfaces: &HttpSurfaces,
) -> LocalResult<()> {
    let context = crate::diagnostics::RequestContext::new(header(&request, "X-Tempo-Request-Id"));
    let mut trace = crate::diagnostics::Span::new("http.request");
    trace.field("request_id", context.id());
    trace.field(
        "method",
        match request.method() {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Head => "HEAD",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Options => "OPTIONS",
            Method::Patch => "PATCH",
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
    mut request: Request,
    surfaces: &HttpSurfaces,
) -> LocalResult<()> {
    let public = match surfaces.assets_for(&request) {
        Ok(assets) => assets,
        Err(status) => return Ok(respond_traced(request, Response::empty(status))?),
    };
    // Older open tabs have strict v5 reply validators. The opt-in changes
    // display only, independent of submission identity and saved receipts.
    let move_details = header(&request, "X-Tempo-Move-Details") == Some("1");
    if matches!(
        request.url(),
        "/api/games/detectors" | "/api/games/detection" | "/api/games/detection-result"
    ) {
        let reply = match (request.method(), request.url()) {
            (&Method::Get, "/api/games/detectors") => Ok(games.detection.discovery()),
            (&Method::Post, "/api/games/detection") => {
                command::<DetectionRequest>(&mut request, MAX_COMMAND_BYTES)
                    .map(|intent| games.detection.start(app, intent))
            }
            (&Method::Post, "/api/games/detection-result") => {
                command::<DetectionRead>(&mut request, MAX_COMMAND_BYTES)
                    .map(|intent| games.detection.read(app, intent))
            }
            _ => Ok(DetectionReply {
                status: 405,
                body: serde_json::json!({"schema_version": 1, "status": "error", "code": "method_not_allowed"}),
            }),
        }.unwrap_or_else(|error| DetectionReply {
            status: 400,
            body: serde_json::json!({"schema_version": 1, "status": "error", "code": error.code}),
        });
        respond_traced(
            request,
            body(
                "application/json; charset=utf-8",
                serde_json::to_vec(&reply.body)?,
            )
            .with_status_code(StatusCode(reply.status))
            .with_header(field("Cache-Control", "no-store")),
        )?;
    } else if matches!(
        request.url(),
        "/api/games"
            | "/api/analysis/evidence"
            | "/api/analysis/result"
            | "/api/games/library"
            | "/api/games/job"
            | "/api/games/import"
            | "/api/games/refresh"
            | "/api/games/choose"
            | "/api/games/reprocess"
            | "/api/games/backfill"
            | "/api/games/backfill/retry"
            | "/api/games/model"
            | "/api/games/gaps"
            | "/api/games/gap-draft"
            | "/api/games/review"
            | "/api/games/gap-analysis"
            | "/api/games/gap-explorer"
            | "/api/study/relevance"
            | "/api/study/game-context"
            | "/api/study/analysis-context"
            | "/api/study/analysis-line"
    ) {
        let (status, reply) = match (request.method(), request.url()) {
            (&Method::Post, "/api/analysis/evidence") => (
                202,
                command::<crate::position_evidence::EvidenceRequest>(
                    &mut request,
                    MAX_COMMAND_BYTES,
                )
                .and_then(|r| games.position_evidence.start(r)),
            ),
            (&Method::Post, "/api/analysis/result") => (
                200,
                command::<crate::position_evidence::EvidenceRead>(&mut request, MAX_COMMAND_BYTES)
                    .and_then(|r| games.position_evidence.read(r)),
            ),
            (&Method::Post, "/api/games/library") => (
                200,
                command::<crate::GameLibraryRead>(&mut request, MAX_COMMAND_BYTES)
                    .and_then(|r| app.game_library(r)),
            ),
            (&Method::Get, "/api/games") => (200, app.game_history()),
            (&Method::Get, "/api/games/job") => (200, Ok(games.refresh.poll(app))),
            (&Method::Get, "/api/games/backfill") => (200, games.backfill.current_status(app)),
            (&Method::Post, "/api/games/backfill") => (
                202,
                command::<BackfillRequest>(&mut request, MAX_COMMAND_BYTES)
                    .and_then(|intent| games.backfill.begin(app, intent)),
            ),
            (&Method::Post, "/api/games/backfill/retry") => (
                202,
                command::<BackfillRetry>(&mut request, MAX_COMMAND_BYTES)
                    .and_then(|intent| games.backfill.retry(app, intent)),
            ),
            (&Method::Get, "/api/games/model") => (200, games.model.status(app)),
            (&Method::Post, "/api/games/model") => (
                202,
                command::<ModelRequest>(&mut request, MAX_COMMAND_BYTES)
                    .and_then(|intent| games.model.start(app, intent)),
            ),
            (&Method::Get, "/api/games/gaps") => (200, app.opponent_gaps(0)),
            (&Method::Post, "/api/games/gaps") => (
                200,
                command::<GapPage>(&mut request, MAX_COMMAND_BYTES)
                    .and_then(|page| app.opponent_gaps(page.offset)),
            ),
            (&Method::Get, "/api/games/gap-analysis") => (200, Ok(games.gap_analysis.status())),
            (&Method::Post, "/api/games/gap-analysis") => (
                202,
                command::<GapAnalysisRequest>(&mut request, MAX_COMMAND_BYTES)
                    .and_then(|intent| games.gap_analysis.start(app, intent)),
            ),
            (&Method::Get, "/api/games/gap-explorer") => (200, Ok(games.gap_explorer.status())),
            (&Method::Post, "/api/games/gap-explorer") => (
                202,
                command::<GapExplorerRequest>(&mut request, MAX_COMMAND_BYTES)
                    .and_then(|intent| games.gap_explorer.start(app, intent)),
            ),
            (&Method::Post, "/api/games/review") => (
                200,
                command::<crate::GameReviewRead>(&mut request, MAX_COMMAND_BYTES)
                    .and_then(|request| app.game_review(request)),
            ),
            (&Method::Post, "/api/games/gap-draft") => (
                200,
                command::<GapRequest>(&mut request, MAX_COMMAND_BYTES)
                    .and_then(|intent| app.game_gap_draft(intent)),
            ),
            (&Method::Post, "/api/study/relevance") => (
                200,
                command::<StudyAttemptRead>(&mut request, MAX_COMMAND_BYTES).and_then(|intent| {
                    app.study_relevance_reason(&intent.dataset_id, &intent.attempt_id)
                }),
            ),
            (&Method::Post, "/api/study/analysis-context") => (
                200,
                command::<StudyAnalysisRead>(&mut request, MAX_COMMAND_BYTES).and_then(|intent| {
                    app.study_analysis_context(
                        &intent.dataset_id,
                        &intent.attempt_id,
                        intent.revision,
                        intent.content_version,
                    )
                }),
            ),
            (&Method::Post, "/api/study/analysis-line") => (
                200,
                command::<StudyAnalysisRead>(&mut request, MAX_COMMAND_BYTES).and_then(|intent| {
                    app.study_analysis_line(
                        &intent.dataset_id,
                        &intent.attempt_id,
                        intent.revision,
                        intent.content_version,
                    )
                    .map(|line| serde_json::json!(line))
                }),
            ),
            (&Method::Post, "/api/study/game-context") => (
                200,
                command::<StudyAttemptRead>(&mut request, MAX_COMMAND_BYTES).and_then(|intent| {
                    let mut reply =
                        app.study_game_context(&intent.dataset_id, &intent.attempt_id)?;
                    if !move_details && let Some(context) = reply["context"].as_object_mut() {
                        context.remove("played_uci");
                    }
                    Ok(reply)
                }),
            ),
            (&Method::Post, "/api/games/import") => (
                200,
                json_command::<GameImport>(&mut request, None)
                    .and_then(|intent| app.import_game_pgn(intent))
                    .map(|report| serde_json::json!(report)),
            ),
            (&Method::Post, "/api/games/choose") => (
                200,
                command::<crate::game_evidence::ChooseGameCorrection>(
                    &mut request,
                    MAX_COMMAND_BYTES,
                )
                .and_then(|intent| app.choose_game_correction(intent))
                .map(|report| serde_json::json!(report)),
            ),
            (&Method::Post, "/api/games/reprocess") => (
                200,
                command::<crate::game_evidence::ReprocessGames>(&mut request, MAX_COMMAND_BYTES)
                    .and_then(|intent| app.reprocess_games(intent))
                    .map(|report| serde_json::json!(report)),
            ),
            (&Method::Post, "/api/games/refresh") => (
                202,
                command::<GameRefresh>(&mut request, MAX_COMMAND_BYTES)
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
            body(
                "application/json; charset=utf-8",
                serde_json::to_vec(&reply)?,
            )
            .with_status_code(StatusCode(status))
            .with_header(field("Cache-Control", "no-store")),
        )?;
    } else if request.method() == &Method::Post && request.url() == "/api/tactic-settings" {
        let reply = command(&mut request, MAX_COMMAND_BYTES)
            .and_then(|intent| tactic_settings::handle(app, profile_store, intent))
            .unwrap_or_else(|error| serde_json::json!(ErrorView::for_content(error)));
        respond_traced(
            request,
            body(
                "application/json; charset=utf-8",
                serde_json::to_vec(&reply)?,
            ),
        )?;
    } else if request.method() == &Method::Post && request.url() == "/api/tactics-collection" {
        let reply = command::<crate::TacticsRead>(&mut request, MAX_COMMAND_BYTES)
            .and_then(|intent| app.read_tactics(intent))
            .map(|content| serde_json::json!(content))
            .unwrap_or_else(|error| serde_json::json!(StudyView::from(error)));
        respond_traced(
            request,
            body(
                "application/json; charset=utf-8",
                serde_json::to_vec(&reply)?,
            ),
        )?;
    } else if request.method() == &Method::Post
        && matches!(
            request.url(),
            "/api/study/review-mark" | "/api/study/saved-reviews"
        )
    {
        let reply = match request.url() {
            "/api/study/review-mark" => command::<SaveStudyReview>(&mut request, MAX_COMMAND_BYTES)
                .and_then(|intent| app.save_study_review(intent)),
            _ => command::<SavedStudyReviewCommand>(&mut request, MAX_COMMAND_BYTES)
                .and_then(|intent| app.saved_study_reviews(intent)),
        }
        .unwrap_or_else(|error| serde_json::json!(StudyView::from(error)));
        respond_traced(
            request,
            body(
                "application/json; charset=utf-8",
                serde_json::to_vec(&reply)?,
            ),
        )?;
    } else if request.method() == &Method::Post && request.url() == "/api/study-progress" {
        let reply = command::<Option<crate::ProgressStamp>>(&mut request, MAX_COMMAND_BYTES)
            .and_then(|known| app.study_progress(known.as_ref()))
            .map(|progress| serde_json::json!(progress))
            .unwrap_or_else(|error| serde_json::json!(StudyView::from(error)));
        respond_traced(
            request,
            body(
                "application/json; charset=utf-8",
                serde_json::to_vec(&reply)?,
            ),
        )?;
    } else if request.method() == &Method::Post && request.url() == "/api/study" {
        let reply = command::<StudyCommand>(&mut request, MAX_COMMAND_BYTES)
            .map(|intent| app.handle(intent))
            .unwrap_or_else(Into::into);
        let reply = if move_details {
            reply
        } else {
            reply.without_move_details()
        };
        respond_traced(
            request,
            body(
                "application/json; charset=utf-8",
                serde_json::to_vec(&reply)?,
            ),
        )?;
    } else if request.method() == &Method::Post
        && matches!(request.url(), "/api/repertoires" | "/api/tactics")
    {
        let reply = if request.url() == "/api/repertoires" {
            content_command(&mut request, |intent: RepertoireCommand| {
                app.handle_repertoire(intent)
            })?
        } else {
            content_command(&mut request, |intent: TacticCaptureCommand| {
                app.handle_tactic_capture(intent)
            })?
        };
        respond_traced(
            request,
            body(
                "application/json; charset=utf-8",
                serde_json::to_vec(&reply)?,
            ),
        )?;
    } else if request.method() == &Method::Get {
        if let Some(asset) = public.get(asset_path(&request)) {
            respond_traced(request, body(asset.mime, asset.bytes.clone()).with_header(
                field("Content-Security-Policy", "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; object-src 'none'; frame-ancestors 'none'"),
            ))?;
        } else {
            respond_traced(request, Response::empty(StatusCode(404)))?;
        }
    } else {
        respond_traced(request, Response::empty(StatusCode(405)))?;
    }
    Ok(())
}

fn content_command<T: DeserializeOwned>(
    request: &mut Request,
    handle: impl FnOnce(T) -> Value,
) -> LocalResult<Value> {
    Ok(match json_command(request, None) {
        Ok(intent) => handle(intent),
        Err(error) => serde_json::to_value(ErrorView::for_content(error))?,
    })
}

fn command<T: DeserializeOwned>(request: &mut Request, max_bytes: usize) -> Result<T, AppError> {
    json_command(request, Some(max_bytes))
}

fn json_command<T: DeserializeOwned>(
    request: &mut Request,
    max_bytes: Option<usize>,
) -> Result<T, AppError> {
    let invalid = |message| AppError::new("invalid_command", message);
    let mime = header(request, "Content-Type").and_then(|value| value.split(';').next());
    if mime != Some("application/json") {
        return Err(invalid("Use an application/json command."));
    }
    let mut body_trace = crate::diagnostics::Span::new("http.read_body");
    let size = request.body_length();
    let mut bytes = Vec::new();
    if let Some(max_bytes) = max_bytes {
        let Some(size) = size.filter(|size| *size <= max_bytes) else {
            body_trace.set_outcome("invalid_size");
            return Err(AppError::new(
                "invalid_command",
                format!("Send a command body of at most {} KiB.", max_bytes / 1024),
            ));
        };
        bytes.resize(size, 0);
        request.as_reader().read_exact(&mut bytes).map_err(|_| {
            body_trace.set_outcome("io_error");
            invalid("Could not read the command.")
        })?;
    } else {
        request.as_reader().read_to_end(&mut bytes).map_err(|_| {
            body_trace.set_outcome("io_error");
            invalid("Could not read the command.")
        })?;
        if size.is_some_and(|size| bytes.len() != size) {
            body_trace.set_outcome("invalid_size");
            return Err(invalid("Could not read the command."));
        }
    }
    body_trace.field("bytes", bytes.len());
    drop(body_trace);
    let mut decode_trace = crate::diagnostics::Span::new("http.decode_body");
    let result = serde_json::from_slice(&bytes).map_err(|_| invalid("The command is invalid."));
    if result.is_err() {
        decode_trace.set_outcome("invalid_json");
    }
    result
}

fn header<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|field| field.field.equiv(name))
        .map(|field| field.value.as_str())
}

fn field(name: &str, value: &str) -> Header {
    Header::from_bytes(name, value).expect("static HTTP header")
}

fn body(mime: &str, bytes: Vec<u8>) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(bytes)
        .with_header(field("Content-Type", mime))
        .with_header(field("Cache-Control", "no-store"))
        .with_header(field("X-Content-Type-Options", "nosniff"))
}

/// Diagnostics only use known route templates; URLs and query strings are never recorded.
fn diagnostic_route(url: &str) -> &'static str {
    match url.split('?').next().unwrap_or_default() {
        "/api/study" => "study",
        "/api/study-progress" => "study_progress",
        "/api/study/relevance" => "study_relevance",
        "/api/study/game-context" => "study_game_context",
        "/api/study/analysis-context" => "study_analysis_context",
        "/api/study/analysis-line" => "study_analysis_line",
        "/api/study/review-mark" => "study_review_mark",
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
        "/api/games/gaps" => "game_gaps",
        "/api/games/gap-draft" => "game_gap_draft",
        "/api/games/review" => "game_review",
        "/api/games/gap-analysis" => "game_gap_analysis",
        "/api/games/gap-explorer" => "game_gap_explorer",
        "/api/games/detectors" => "game_detectors",
        "/api/games/detection" => "game_detection",
        "/api/games/detection-result" => "game_detection_result",
        "/api/analysis/evidence" => "analysis_evidence",
        "/api/analysis/result" => "analysis_result",
        "/" | "/index.html" => "ui_index",
        path if path.starts_with("/assets/") => "ui_asset",
        _ => "other",
    }
}

/// A completed write records transport delivery only, never command acceptance.
fn respond_traced<R: std::io::Read>(
    request: Request,
    mut response: Response<R>,
) -> std::io::Result<()> {
    let mut trace = crate::diagnostics::Span::new("http.write_response");
    let status = response.status_code().0;
    trace.field("status_code", status);
    if let Some(id) = crate::diagnostics::current_request_id() {
        response.add_header(field("X-Tempo-Request-Id", &id));
    }
    if let Some(id) = crate::diagnostics::run_id() {
        response.add_header(field("X-Tempo-Run-Id", &id));
    }
    let result = request.respond(response);
    let outcome = if result.is_ok() {
        "written"
    } else {
        "io_error"
    };
    trace.set_outcome(outcome);
    crate::diagnostics::emit(
        "http.response",
        serde_json::json!({
            "operation":"http.write_response", "status_code":status, "outcome":outcome
        }),
    );
    result
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
