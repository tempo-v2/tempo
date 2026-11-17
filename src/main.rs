use std::io::{self, BufRead, Write};
use tempo::{
    AppError, Tempo,
    wire::{StudyCommand, StudyView},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() != Some("stdio") {
        eprintln!("Tempo interface demo: cargo run -p tempo -- stdio");
        eprintln!(
            "One JSON command per line; begin with {{\"command\":\"next\"}}. State is in memory."
        );
        return Ok(());
    }
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
