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
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&path)?,
        Ok(_) => anyhow::bail!("Tactic worker path is occupied"),
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    let listener = UnixListener::bind(&path)?;
    let database = database.to_owned();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            if let Err(error) = handle(stream, database.clone(), writer.clone()).await {
                eprintln!("Tactic worker connection: {error:#}");
            }
        }
    });
    Ok(())
}

async fn handle(mut stream: UnixStream, database: PathBuf, writer: Writer) -> Result<()> {
    let request = timeout(IO_DEADLINE, wire::read(&mut stream)).await??;
    let reply = tokio::task::spawn_blocking(move || dispatch(request, &database, &writer)).await?;
    let response = match reply {
        Ok(reply) => reply,
        Err(error) => {
            eprintln!("Tactic worker request: {error:#}");
            json!({"status":"error"})
        }
    };
    timeout(IO_DEADLINE, wire::send(&mut stream, &response)).await??;
    Ok(())
}

fn dispatch(request: Request, database: &Path, writer: &Writer) -> Result<Value> {
    let now = chrono::Utc::now().timestamp();
    match request {
        Request::Next => {
            let db = Database::read_with_busy_timeout(database, Duration::ZERO)?;
            let snapshot = db.unchecked_transaction()?;
            Ok(json!({"status":"ok","work":capture_publication::next(&snapshot,now)?}))
        }
        Request::Status => {
            let db = Database::read_with_busy_timeout(database, Duration::ZERO)?;
            Ok(json!({"status":"ok","counts":capture_publication::status(&db)?}))
        }
        Request::Submit { work, evidence } => {
            let evidence = serde_json::from_value(evidence)?;
            let outcome = writer.background_until(Instant::now() + IO_DEADLINE, move |db| {
                capture_publication::submit(db, &work, &evidence)
            })?;
            Ok(json!({"status":"ok","outcome":outcome}))
        }
        Request::Retry { work, error } => {
            writer.background_until(Instant::now() + IO_DEADLINE, move |db| {
                capture_publication::retry(db, &work, error, now)
            })?;
            Ok(json!({"status":"ok"}))
        }
    }
}
