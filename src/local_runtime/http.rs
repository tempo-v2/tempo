//! Serve public UI artifacts and serialize commands through a single Tempo app.
use super::{LocalResult, tactic_settings};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    net::Ipv4Addr,
    path::Path,
    time::{Duration, Instant},
};
use tempo::{
    AppError, Tempo,
    game_backfill::{BackfillCoordinator, BackfillRequest, BackfillRetry},
    game_evidence::{GameImport, GameJobs, GameRefresh},
    game_model::{GameModel, GapRequest, ModelRequest},
    gap_analysis::{GapAnalysis, GapAnalysisRequest},
    gap_explorer::{GapExplorer, GapExplorerRequest},
    wire::{
        ErrorView, MAX_CONTENT_COMMAND_BYTES, RepertoireCommand, StudyCommand, StudyView,
        TacticCaptureCommand,
    },
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
) -> LocalResult<()> {
    let public = build_files(ui)?;
    let server = Server::http((Ipv4Addr::LOCALHOST, port))?;
    let address = server.server_addr().to_string();
    let origin = format!("http://{address}");
    println!("Tempo study at {origin}");
    let explorer = std::sync::Arc::new(tempo::explorer::ExplorerClient::new(database));
    let mut games = Games {
        refresh: GameJobs::start()?,
        backfill: BackfillCoordinator::start(app)?,
        model: GameModel::new(database)?,
        gap_analysis: GapAnalysis::new(database),
        position_evidence: tempo::position_evidence::PositionEvidence::new(
            database,
            explorer.clone(),
        ),
        gap_explorer: GapExplorer::with_client(explorer),
    };
    let mut model_marker = None;
    let mut next_model_check = Instant::now();
    loop {
        if Instant::now() >= next_model_check {
            let scheduled = (|| -> LocalResult<()> {
                let marker = profile_store.model_content_marker()?;
                if model_marker.as_ref() != Some(&marker) {
                    games.model.project_cached(marker.0.clone())?;
                    model_marker = Some(marker);
                }
                Ok(())
            })();
            if let Err(error) = scheduled {
                eprintln!("Tempo model scheduling: {error}");
            }
            next_model_check = Instant::now() + Duration::from_millis(250);
        }
        games.refresh.poll(app);
        if !games.refresh.active() {
            games.backfill.poll(app);
        }
        let Some(request) = server.recv_timeout(Duration::from_millis(250))? else {
            continue;
        };
        if let Err(error) = respond(
            app,
            profile_store,
            &mut games,
            request,
            &public,
            &address,
            &origin,
        ) {
            eprintln!("Tempo HTTP: {error}");
        }
    }
}

struct Games {
    refresh: GameJobs,
    backfill: BackfillCoordinator,
    model: GameModel,
    gap_analysis: GapAnalysis,
    gap_explorer: GapExplorer,
    position_evidence: tempo::position_evidence::PositionEvidence,
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
    mut request: Request,
    public: &BTreeMap<String, Asset>,
    address: &str,
    origin: &str,
) -> LocalResult<()> {
    // Older open tabs have strict v5 reply validators. The opt-in changes
    // display only, independent of submission identity and saved receipts.
    let move_details = header(&request, "X-Tempo-Move-Details") == Some("1");
    if header(&request, "Host") != Some(address)
        || header(&request, "Origin").is_some_and(|value| value != origin)
    {
        request.respond(Response::empty(StatusCode(403)))?;
    } else if matches!(
        request.url(),
        "/api/games"
            | "/api/analysis/evidence"
            | "/api/analysis/result"
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
            | "/api/games/gap-analysis"
            | "/api/games/gap-explorer"
            | "/api/study/relevance"
            | "/api/study/game-context"
            | "/api/study/analysis-context"
    ) {
        let (status, reply) = match (request.method(), request.url()) {
            (&Method::Post, "/api/analysis/evidence") => (
                202,
                command::<tempo::position_evidence::EvidenceRequest>(
                    &mut request,
                    MAX_COMMAND_BYTES,
                )
                .and_then(|r| games.position_evidence.start(r)),
            ),
            (&Method::Post, "/api/analysis/result") => (
                200,
                command::<tempo::position_evidence::EvidenceRead>(&mut request, MAX_COMMAND_BYTES)
                    .and_then(|r| games.position_evidence.read(r)),
            ),
            (&Method::Get, "/api/games") => (200, app.game_history()),
            (&Method::Get, "/api/games/job") => (200, Ok(games.refresh.poll(app))),
            (&Method::Get, "/api/games/backfill") => (200, Ok(games.backfill.status())),
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
                    app.study_analysis_context(&intent.dataset_id, &intent.attempt_id, intent.revision, intent.content_version)
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
                command::<GameImport>(&mut request, MAX_CONTENT_COMMAND_BYTES)
                    .and_then(|intent| app.import_game_pgn(intent))
                    .map(|report| serde_json::json!(report)),
            ),
            (&Method::Post, "/api/games/choose") => (
                200,
                command::<tempo::game_evidence::ChooseGameCorrection>(
                    &mut request,
                    MAX_COMMAND_BYTES,
                )
                .and_then(|intent| app.choose_game_correction(intent))
                .map(|report| serde_json::json!(report)),
            ),
            (&Method::Post, "/api/games/reprocess") => (
                200,
                command::<tempo::game_evidence::ReprocessGames>(&mut request, MAX_COMMAND_BYTES)
                    .and_then(|intent| app.reprocess_games(intent))
                    .map(|report| serde_json::json!(report)),
            ),
            (&Method::Post, "/api/games/refresh") => (
                202,
                command::<GameRefresh>(&mut request, MAX_COMMAND_BYTES).and_then(|intent| {
                    games.refresh.refresh(app, intent, !games.backfill.active())
                }),
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
        request.respond(
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
        request.respond(body(
            "application/json; charset=utf-8",
            serde_json::to_vec(&reply)?,
        ))?;
    } else if request.method() == &Method::Post && request.url() == "/api/tactics-collection" {
        let reply = command::<tempo::TacticsRead>(&mut request, MAX_COMMAND_BYTES)
            .and_then(|intent| app.read_tactics(intent))
            .map(|content| serde_json::json!(content))
            .unwrap_or_else(|error| serde_json::json!(StudyView::from(error)));
        request.respond(body(
            "application/json; charset=utf-8",
            serde_json::to_vec(&reply)?,
        ))?;
    } else if request.method() == &Method::Post && request.url() == "/api/study-progress" {
        let reply = command::<Option<tempo::ProgressStamp>>(&mut request, MAX_COMMAND_BYTES)
            .and_then(|known| app.study_progress(known.as_ref()))
            .map(|progress| serde_json::json!(progress))
            .unwrap_or_else(|error| serde_json::json!(StudyView::from(error)));
        request.respond(body(
            "application/json; charset=utf-8",
            serde_json::to_vec(&reply)?,
        ))?;
    } else if request.method() == &Method::Post && request.url() == "/api/study" {
        let reply = command::<StudyCommand>(&mut request, MAX_COMMAND_BYTES)
            .map(|intent| app.handle(intent))
            .unwrap_or_else(Into::into);
        let reply = if move_details {
            reply
        } else {
            reply.without_move_details()
        };
        request.respond(body(
            "application/json; charset=utf-8",
            serde_json::to_vec(&reply)?,
        ))?;
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
        request.respond(body(
            "application/json; charset=utf-8",
            serde_json::to_vec(&reply)?,
        ))?;
    } else if request.method() == &Method::Get {
        let url = request.url().split('?').next().unwrap_or_default();
        if let Some(asset) = public.get(url) {
            request.respond(body(asset.mime, asset.bytes.clone()).with_header(
                field("Content-Security-Policy", "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; object-src 'none'; frame-ancestors 'none'"),
            ))?;
        } else {
            request.respond(Response::empty(StatusCode(404)))?;
        }
    } else {
        request.respond(Response::empty(StatusCode(405)))?;
    }
    Ok(())
}

fn content_command<T: DeserializeOwned>(
    request: &mut Request,
    handle: impl FnOnce(T) -> Value,
) -> LocalResult<Value> {
    Ok(match command(request, MAX_CONTENT_COMMAND_BYTES) {
        Ok(intent) => handle(intent),
        Err(error) => serde_json::to_value(ErrorView::for_content(error))?,
    })
}

fn command<T: DeserializeOwned>(request: &mut Request, max_bytes: usize) -> Result<T, AppError> {
    let invalid = |message| AppError::new("invalid_command", message);
    let mime = header(request, "Content-Type").and_then(|value| value.split(';').next());
    if mime != Some("application/json") {
        return Err(invalid("Use an application/json command."));
    }
    let Some(size) = request.body_length().filter(|size| *size <= max_bytes) else {
        return Err(AppError::new(
            "invalid_command",
            format!("Send a command body of at most {} KiB.", max_bytes / 1024),
        ));
    };
    let mut bytes = vec![0; size];
    request
        .as_reader()
        .read_exact(&mut bytes)
        .map_err(|_| invalid("Could not read the command."))?;
    serde_json::from_slice(&bytes).map_err(|_| invalid("The command is invalid."))
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
