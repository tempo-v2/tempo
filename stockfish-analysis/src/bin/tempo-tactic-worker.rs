#[path = "../../../src/tactic_worker_protocol.rs"]
mod wire;

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    time::Duration,
};
use tempo_stockfish_analysis::{Analysis, Coverage, Error, Options, Request, Stockfish};
use tokio::{
    net::UnixStream,
    time::{sleep, timeout},
};
use wire::{Failure, Request as Message, Work};

async fn request(path: &Path, message: &Message) -> Result<Value> {
    let deadline = if matches!(message, Message::Status) {
        Duration::from_millis(250)
    } else {
        Duration::from_secs(5)
    };
    timeout(deadline, async {
        let mut stream = UnixStream::connect(path).await?;
        wire::send(&mut stream, message).await?;
        Ok(wire::read(&mut stream).await?)
    })
    .await?
}

fn accepted(reply: &Value) -> Result<()> {
    ensure!(
        reply["status"] == "ok",
        "Tempo did not accept the tactic worker request"
    );
    Ok(())
}

fn failure(error: Error) -> Failure {
    match error {
        Error::InvalidRequest { .. } => Failure::InvalidRequest,
        Error::Busy => Failure::Busy,
        Error::EngineUnavailable => Failure::EngineUnavailable,
        Error::Timeout => Failure::Timeout,
        Error::Cancelled => Failure::Cancelled,
        Error::Protocol => Failure::Protocol,
        Error::Incomplete => Failure::Incomplete,
    }
}

async fn report(path: &Path) -> Result<()> {
    let reply = request(path, &Message::Status).await?;
    accepted(&reply)?;
    println!("{}", reply["counts"]);
    Ok(())
}

async fn turn(path: &Path, engine: &Stockfish) -> Result<bool> {
    let mut reply = request(path, &Message::Next).await?;
    accepted(&reply)?;
    let Some(work) = serde_json::from_value::<Option<Work>>(reply["work"].take())? else {
        return Ok(false);
    };
    let evidence = engine
        .analyze(Request {
            fen: work.fen.clone(),
            options: Options {
                depth: work.depth,
                coverage: Coverage::All {},
            },
        })
        .await;
    let message = match evidence {
        Ok(analysis @ Analysis::Evaluated { .. }) => {
            let mut evidence = serde_json::to_value(analysis)?;
            // Only root moves and scores cross this boundary.
            for line in evidence["lines"]
                .as_array_mut()
                .context("Missing scored roots")?
            {
                line.as_object_mut()
                    .context("Invalid scored root")?
                    .remove("variation");
            }
            // Both worker modes keep the original token and analysis until a definitive reply.
            let submission = Message::Submit {
                work: work.clone(),
                evidence,
            };
            loop {
                let reply = request(path, &submission).await;
                match reply
                    .and_then(|value| Ok(serde_json::from_value::<wire::SubmitReply>(value)?))
                {
                    Ok(wire::SubmitReply::Ok { outcome }) => {
                        let _ = outcome;
                        return Ok(true);
                    }
                    Ok(wire::SubmitReply::StaleWork) => return Ok(true),
                    Ok(wire::SubmitReply::InvalidEvidence) => {
                        break Message::Retry {
                            work,
                            error: Failure::InvalidEvidence,
                        };
                    }
                    Err(_) => sleep(Duration::from_secs(2)).await,
                }
            }
        }
        Ok(Analysis::Terminal { .. }) => Message::Retry {
            work,
            error: Failure::InvalidEvidence,
        },
        Err(Error::EngineUnavailable) => anyhow::bail!("Stockfish is unavailable"),
        Err(error) => Message::Retry {
            work,
            error: failure(error),
        },
    };
    accepted(&request(path, &message).await?)?;
    Ok(true)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (operation, database, executable) = match args.as_slice() {
        [operation, database] if operation == "status" => (operation, database, None),
        [operation, database, executable] if operation == "drain" || operation == "watch" => {
            (operation, database, Some(PathBuf::from(executable)))
        }
        _ => anyhow::bail!(
            "usage: tempo-tactic-worker status DATABASE | (drain|watch) DATABASE STOCKFISH"
        ),
    };
    let database = fs::canonicalize(database)?;
    let path = wire::socket_path(&database);
    let status = wire::status_path(&database);
    let Some(executable) = executable else {
        return report(&status).await;
    };
    let _lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_file_name("worker.lock"))?;
    _lock
        .try_lock_exclusive()
        .context("Another tactic worker owns this database")?;
    let engine = Stockfish::new(executable);
    loop {
        match turn(&path, &engine).await {
            Ok(true) => {}
            Ok(false) if operation == "drain" => return report(&status).await,
            Err(err) if operation == "drain" => return Err(err),
            Ok(false) => sleep(Duration::from_secs(5)).await,
            Err(err) => {
                eprintln!("{err:#}");
                sleep(Duration::from_secs(30)).await;
            }
        }
    }
}
