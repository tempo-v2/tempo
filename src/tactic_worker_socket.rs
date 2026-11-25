//! Private evidence handoff. The server never loads or spawns an engine.
use crate::{
    capture_publication,
    db::Database,
    tactic_worker_protocol::{self as wire, Request},
    writer::Writer,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs::{self, DirBuilder},
    io::ErrorKind,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::{
    net::{UnixListener, UnixStream},
    time::timeout,
};

const IO_DEADLINE: Duration = Duration::from_secs(2);

pub fn start(database: &Path, writer: Writer) -> Result<()> {
    let path = wire::socket_path(database);
    let directory = path.parent().context("Missing tactic worker directory")?;
    match DirBuilder::new().mode(0o700).create(directory) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err.into()),
    }
    let metadata = fs::symlink_metadata(directory)?;
    ensure!(
        metadata.is_dir()
            && metadata.mode() & 0o777 == 0o700
            && metadata.uid() == fs::metadata(database)?.uid(),
        "Unsafe tactic worker directory"
    );
    // The caller holds the exclusive database lock through shutdown.
    let work = bind(&path)?;
    listen(work, database.to_owned(), writer.clone(), false);
    match bind(&wire::status_path(database)) {
        Ok(status) => listen(status, database.to_owned(), writer, true),
        Err(error) => eprintln!("Tactic worker status unavailable: {error:#}"),
    }
    Ok(())
}

fn bind(path: &Path) -> Result<UnixListener> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path)?,
        Ok(_) => anyhow::bail!("Tactic worker path is occupied"),
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    Ok(UnixListener::bind(path)?)
}

fn listen(listener: UnixListener, database: PathBuf, writer: Writer, status: bool) {
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            if let Err(error) = handle(stream, database.clone(), writer.clone(), status).await {
                eprintln!("Tactic worker connection: {error:#}");
            }
        }
    });
}

async fn handle(
    mut stream: UnixStream,
    database: PathBuf,
    writer: Writer,
    status: bool,
) -> Result<()> {
    let budget = if status {
        Duration::from_millis(250)
    } else {
        IO_DEADLINE
    };
    let deadline = Instant::now() + budget;
    let request = tokio::time::timeout_at(deadline.into(), wire::read(&mut stream)).await??;
    if status != matches!(request, Request::Status) {
        tokio::time::timeout_at(
            deadline.into(),
            wire::send(&mut stream, &json!({"status":"unsupported"})),
        )
        .await??;
        return Ok(());
    }
    let read_only = matches!(request, Request::Status | Request::Next);
    let reply =
        tokio::task::spawn_blocking(move || dispatch(request, &database, &writer, deadline))
            .await?;
    let response = match reply {
        Ok(reply) => reply,
        Err(error) if error.is::<capture_publication::InvalidEvidence>() => {
            json!({"status":"invalid_evidence"})
        }
        Err(error) if error.is::<crate::writer::Unavailable>() => json!({"status":"unavailable"}),
        Err(error) => {
            eprintln!("Tactic worker request: {error:#}");
            json!({"status":"error"})
        }
    };
    if read_only {
        tokio::time::timeout_at(deadline.into(), wire::send(&mut stream, &response)).await??;
    } else {
        timeout(IO_DEADLINE, wire::send(&mut stream, &response)).await??;
    }
    Ok(())
}

fn dispatch(
    request: Request,
    database: &Path,
    writer: &Writer,
    deadline: Instant,
) -> Result<Value> {
    ensure!(Instant::now() < deadline, crate::writer::Unavailable);
    let now = chrono::Utc::now().timestamp();
    match request {
        Request::Next => {
            let db = Database::read_with_busy_timeout(database, Duration::ZERO)?;
            db.progress_handler(100, Some(move || Instant::now() >= deadline));
            let snapshot = db.unchecked_transaction()?;
            Ok(json!({"status":"ok","work":capture_publication::next(&snapshot,now)?}))
        }
        Request::Status => {
            let db = Database::read_with_busy_timeout(database, Duration::ZERO)?;
            db.progress_handler(100, Some(move || Instant::now() >= deadline));
            Ok(json!({"status":"ok","counts":capture_publication::status(&db)?}))
        }
        Request::Submit { work, evidence } => {
            let Ok(evidence) = serde_json::from_value(evidence) else {
                return Ok(json!({"status":"invalid_evidence"}));
            };
            let outcome = writer.background_until(deadline, move |db| {
                capture_publication::submit(db, &work, &evidence)
            })?;
            Ok(json!({"status":"ok","outcome":outcome}))
        }
        Request::Retry { work, error } => {
            writer.background_until(deadline, move |db| {
                capture_publication::retry(db, &work, error, now)
            })?;
            Ok(json!({"status":"ok"}))
        }
    }
}
