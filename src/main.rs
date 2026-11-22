use std::{error::Error, path::Path};
use tempo::local_runtime;

fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.as_slice() {
        [mode] if mode == "stdio" => Err("The standalone stdio workflow has been retired.".into()),
        [mode, database] if mode == "init" => local_runtime::init(Path::new(database)),
        [mode, database, repertoire, tactics] if mode == "configure" => {
            local_runtime::configure(Path::new(database), repertoire.parse()?, tactics.parse()?, None)
        }
        [mode, database, repertoire, tactics, more] if mode == "configure" => {
            local_runtime::configure(Path::new(database), repertoire.parse()?, tactics.parse()?, Some(more.parse()?))
        }
        [mode, database, normalized] if mode == "import-tactics" => {
            local_runtime::import(Path::new(database), Path::new(normalized))
        }
        [mode, ..] if matches!(mode.as_str(),
            "backfill-game-capture-origins" | "backfill-study-lead-ins" | "split-black-repertoire"
        ) => Err("This one-off Tempo maintenance command has been retired.".into()),
        [mode, database, ui, port] if mode == "serve" => {
            local_runtime::serve(Path::new(database), Path::new(ui), port.parse()?)
        }
        _ => Err(
            "Usage: tempo init DATABASE | configure DATABASE NEW_OPENINGS NEW_TACTICS [MORE_TACTICS] | import-tactics DATABASE NORMALIZED.json | serve DATABASE UI_DIST PORT"
                .into(),
        ),
    }
}
