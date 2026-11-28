use super::*;

pub fn start(path: PathBuf, writer: crate::writer::Writer, shutdown: watch::Receiver<bool>) {
    if let Err(error) = std::thread::Builder::new()
        .name("tempo-forecast".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let result = runtime.and_then(|runtime| {
                runtime
                    .block_on(serve(path, writer, shutdown))
                    .map_err(std::io::Error::other)
            });
            if let Err(error) = result {
                eprintln!("Daily forecast unavailable: {error}");
            }
        })
    {
        eprintln!("Daily forecast unavailable: {error}");
    }
}

async fn serve(
    path: PathBuf,
    writer: crate::writer::Writer,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt},
    };
    let directory = path.with_extension("dq");
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    let dir = fs::symlink_metadata(&directory)?;
    let database = fs::metadata(&path)?;
    ensure!(
        dir.is_dir() && dir.mode() & 0o777 == 0o700 && dir.uid() == database.uid(),
        "Daily forecast requires an owner-only directory"
    );
    let socket = socket(&directory);
    match fs::symlink_metadata(&socket) {
        Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == dir.uid() => {
            fs::remove_file(&socket)?
        }
        Ok(_) => anyhow::bail!("Forecast socket path is occupied"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let listener = tokio::net::UnixListener::bind(&socket)?;
    loop {
        let (stream, _) = tokio::select! {
            _ = shutdown.wait_for(|done| *done) => break,
            result = listener.accept() => result?,
        };
        if stream.peer_cred().is_ok_and(|cred| cred.uid() == dir.uid())
            && let Err(error) = handle(stream, path.clone(), writer.clone(), &mut shutdown).await
        {
            eprintln!("Daily forecast handoff: {error:#}");
        }
    }
    // A successor removes the stale socket while holding the database lock.
    Ok(())
}

async fn handle(
    mut stream: UnixStream,
    path: PathBuf,
    writer: crate::writer::Writer,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<()> {
    let limit = Instant::now() + FRAME_TIME;
    let bytes = tokio::select! {
        _ = shutdown.wait_for(|done| *done) => return Ok(()),
        result = read_frame(&mut stream, limit) => result?,
    };
    let received = chrono::Utc::now().timestamp();
    let reply = tokio::task::spawn_blocking(move || {
        use crate::daily_store::forecast;
        let Ok(Envelope { version, command }) = serde_json::from_slice(&bytes) else {
            return Ok::<_, anyhow::Error>(serde_json::to_vec(&Reply::Delivery(
                Delivery::Invalid,
            ))?);
        };
        let result = if version != VERSION {
            Ok(Reply::Unsupported)
        } else {
            match command {
                CommandBody::ClaimToday { profile } => {
                    forecast::claim_today(&path, &writer, &profile, limit.into_std())
                        .map(|claim| claim.map_or(Reply::NoWork, Reply::Work))
                }
                CommandBody::SubmitForecast { ticket, outcome } => {
                    forecast::submit(&path, &writer, ticket, outcome, received).map(Reply::Delivery)
                }
            }
        };
        let reply = match result {
            Ok(reply) => reply,
            Err(error) if error.is::<crate::writer::Unavailable>() => Reply::Busy,
            Err(error) => return Err(error),
        };
        Ok::<_, anyhow::Error>(serde_json::to_vec(&reply)?)
    })
    .await??;
    write_frame(&mut stream, &reply, Instant::now() + FRAME_TIME).await
}
