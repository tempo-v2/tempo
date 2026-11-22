use std::{error::Error, path::Path};
use tempo::local_runtime;

fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.as_slice() {
        [mode] if mode == "stdio" => Err("The standalone stdio workflow has been retired.".into()),
        [mode, database] if mode == "init" || mode == "init-practice" => {
            local_runtime::init(Path::new(database))
        }
        [mode, database, repertoire, tactics] if mode == "configure" => {
            local_runtime::configure(Path::new(database), repertoire.parse()?, tactics.parse()?, None)
        }
        [mode, database, repertoire, tactics, more] if mode == "configure" => {
            local_runtime::configure(Path::new(database), repertoire.parse()?, tactics.parse()?, Some(more.parse()?))
        }
        [mode, database, normalized] if mode == "import-tactics" => {
            local_runtime::import(Path::new(database), Path::new(normalized))
        }
        [mode, database, transfer, args @ ..] if mode == "backfill-game-capture-origins" => {
            use local_runtime::GameCaptureBackfill;
            let operation = match args {
                [] => GameCaptureBackfill::Preview,
                [apply] if apply == "--apply" => GameCaptureBackfill::Apply,
                _ => return Err("Usage: tempo backfill-game-capture-origins DATABASE TRANSFER.json [--apply]".into()),
            };
            local_runtime::backfill_game_capture_origins(Path::new(database), Path::new(transfer), operation)
        }
        [mode, database, args @ ..] if mode == "backfill-study-lead-ins" => {
            let (transfer, apply) = match args {
                [] => (None, false),
                [apply] if apply == "--apply" => (None, true),
                [path] => (Some(Path::new(path)), false),
                [path, apply] if apply == "--apply" => (Some(Path::new(path)), true),
                _ => return Err("Usage: tempo backfill-study-lead-ins DATABASE [TRANSFER.json] [--apply]".into()),
            };
            local_runtime::backfill_study_lead_ins(Path::new(database), transfer, apply)
        }
        [mode, database, id, args @ ..] if mode == "split-black-repertoire" => {
            let apply = match args {
                [] => None,
                [action, token] if action == "--apply" => Some(token.as_str()),
                _ => return Err("Usage: tempo split-black-repertoire DATABASE ID [--apply TOKEN]".into()),
            };
            local_runtime::split_black_repertoire(Path::new(database), id, apply)
        }
        [mode, database, ui, port] if mode == "serve" => {
            local_runtime::serve(Path::new(database), Path::new(ui), port.parse()?)
        }
        _ => Err(
            "Usage: tempo init DATABASE | configure DATABASE NEW_OPENINGS NEW_TACTICS [MORE_TACTICS] | import-tactics DATABASE NORMALIZED.json | backfill-game-capture-origins DATABASE TRANSFER.json [--apply] | backfill-study-lead-ins DATABASE [TRANSFER.json] [--apply] | split-black-repertoire DATABASE ID [--apply TOKEN] | serve DATABASE UI_DIST PORT"
                .into(),
        ),
    }
}
