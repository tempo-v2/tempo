mod local_runtime;

use std::{
    error::Error,
    io::{self, BufRead, Write},
    path::Path,
};
use tempo::{
    AppError, Tempo,
    wire::{StudyCommand, StudyView},
};

fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.as_slice() {
        [mode] if mode == "stdio" => stdio(),
        [mode, database] if mode == "init" || mode == "init-practice" => {
            local_runtime::init(Path::new(database))
        }
        [mode, database, repertoire, tactics] if mode == "configure" => {
            local_runtime::configure(Path::new(database), repertoire.parse()?, tactics.parse()?)
        }
        [mode, database, normalized] if mode == "import-tactics" => {
            local_runtime::import(Path::new(database), Path::new(normalized))
        }
        [mode, database, ui, port] if mode == "serve" => {
            local_runtime::serve(Path::new(database), Path::new(ui), port.parse()?)
        }
        _ => Err(
            "Usage: tempo stdio | init DATABASE | configure DATABASE NEW_OPENINGS NEW_TACTICS | import-tactics DATABASE NORMALIZED.json | serve DATABASE UI_DIST PORT"
                .into(),
        ),
    }
}

fn stdio() -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut app = Tempo::demo()?;
    let mut output = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let view = match serde_json::from_str::<StudyCommand>(&line) {
            Ok(command) => app.handle(command),
            Err(error) => StudyView::from(AppError::new("invalid_command", error.to_string())),
        };
        serde_json::to_writer(&mut output, &view)?;
        writeln!(&mut output)?;
        output.flush()?;
    }
    Ok(())
}
