use serde::Serialize;
use std::{
    io::{self, Read},
    path::PathBuf,
    process::ExitCode,
};
use tempo_stockfish_analysis::{Error, Request, Stockfish};

const MAX_REQUEST_BYTES: usize = 16 * 1024;

#[derive(Serialize)]
struct Failure {
    status: &'static str,
    error: Error,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let result = run().await;
    let success = result.is_ok();
    let serialized = match result {
        Ok(analysis) => serde_json::to_string(&analysis),
        Err(error) => serde_json::to_string(&Failure {
            status: "error",
            error,
        }),
    };
    match serialized {
        Ok(json) => println!("{json}"),
        Err(_) => return ExitCode::FAILURE,
    }
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn run() -> Result<tempo_stockfish_analysis::Analysis, Error> {
    let invalid = |reason| Error::InvalidRequest { reason };
    let mut args = std::env::args_os().skip(1);
    let executable = args
        .next()
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or(invalid(
            "pass the absolute Stockfish executable path as the sole argument",
        ))?;
    if args.next().is_some() {
        return Err(invalid(
            "pass the absolute Stockfish executable path as the sole argument",
        ));
    }
    let mut bytes = Vec::new();
    io::stdin()
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid("could not read the JSON request"))?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(invalid("JSON request exceeds 16 KiB"));
    }
    let request: Request =
        serde_json::from_slice(&bytes).map_err(|_| invalid("invalid JSON request"))?;
    Stockfish::new(executable)
        .analyze_until(request, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}
