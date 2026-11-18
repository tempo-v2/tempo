//! Serve public UI artifacts and serialize commands through a single Tempo app.
use super::{LocalResult, tactic_settings};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{collections::BTreeMap, fs, net::Ipv4Addr, path::Path, time::Duration};
use tempo::{
    AppError, Tempo,
    game_evidence::{GameImport, GameJobs, GameRefresh},
    wire::{ErrorView, RepertoireCommand, StudyCommand, StudyView, TacticCaptureCommand},
};
use tempo_store::SqliteStore;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_CONTENT_COMMAND_BYTES: usize = 2 * 1024 * 1024;

pub(super) fn serve(
    app: &mut Tempo,
    profile_store: &SqliteStore,
    ui: &Path,
    port: u16,
) -> LocalResult<()> {
    let public = build_files(ui)?;
    let server = Server::http((Ipv4Addr::LOCALHOST, port))?;
    let address = server.server_addr().to_string();
    let origin = format!("http://{address}");
    println!("Tempo study at {origin}");
    let mut games = GameJobs::start()?;
    loop {
        games.poll(app);
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
    games: &mut GameJobs,
    mut request: Request,
    public: &BTreeMap<String, Asset>,
    address: &str,
    origin: &str,
) -> LocalResult<()> {
    if header(&request, "Host") != Some(address)
        || header(&request, "Origin").is_some_and(|value| value != origin)
    {
        request.respond(Response::empty(StatusCode(403)))?;
    } else if matches!(
        request.url(),
        "/api/games" | "/api/games/job" | "/api/games/import" | "/api/games/refresh"
    ) {
        let (status, reply) = match (request.method(), request.url()) {
            (&Method::Get, "/api/games") => (200, app.game_history()),
            (&Method::Get, "/api/games/job") => (200, Ok(games.poll(app))),
            (&Method::Post, "/api/games/import") => (
                200,
                command::<GameImport>(&mut request, MAX_CONTENT_COMMAND_BYTES)
                    .and_then(|intent| app.import_game_pgn(intent))
                    .map(|report| serde_json::json!(report)),
            ),
            (&Method::Post, "/api/games/refresh") => (
                202,
                command::<GameRefresh>(&mut request, MAX_COMMAND_BYTES)
                    .and_then(|intent| games.refresh(app, intent)),
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
