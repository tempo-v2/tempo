use anyhow::{Result, ensure};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Instant,
};

fn directory(db: &Path) -> PathBuf {
    db.with_extension("uploads")
}

pub struct Staged {
    path: PathBuf,
    id: String,
    committed: bool,
}

impl Staged {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn stage(db: &Path, pgn: &str) -> Result<Staged> {
    ensure!(
        !pgn.trim().is_empty() && pgn.len() <= crate::jobs::MAX_PGN,
        "enter PGN up to 64 MiB"
    );
    let dir = directory(db);
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&dir)?;
    let id = format!("{:032x}", rand::random::<u128>());
    let path = dir.join(&id);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let staged = Staged {
        path,
        id,
        committed: false,
    };
    file.write_all(pgn.as_bytes())?;
    file.sync_all()?;
    File::open(dir)?.sync_all()?;
    Ok(staged)
}

pub fn reclaim(db: &Path, state: &rusqlite::Connection) -> Result<()> {
    let dir = directory(db);
    if !dir.is_dir() {
        return Ok(());
    }
    let mut used = state.prepare("SELECT EXISTS(SELECT 1 FROM jobs WHERE upload_file=?1)")?;
    for file in fs::read_dir(dir)? {
        let file = file?;
        let name = file.file_name();
        let Some(id) = name.to_str() else { continue };
        // A current request can stage a file before its job is visible to readers.
        let old = file
            .metadata()?
            .modified()?
            .elapsed()
            .is_ok_and(|age| age.as_secs() >= 86_400);
        if old
            && path(db, id).is_ok()
            && file.file_type()?.is_file()
            && !used.query_row([id], |row| row.get::<_, bool>(0))?
        {
            fs::remove_file(file.path())?;
        }
    }
    Ok(())
}

fn path(db: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid upload identity"
    );
    Ok(directory(db).join(id))
}

pub fn open(db: &Path, id: &str, deadline: Instant) -> Result<StagedInput> {
    ensure!(Instant::now() < deadline, "Games turn timed out.");
    Ok(StagedInput(File::open(path(db, id)?)?, deadline))
}

/// File reads during parsing observe the original monotonic turn deadline.
pub struct StagedInput(File, Instant);

impl Read for StagedInput {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        let n = self.0.read(bytes)?;
        self.check()?;
        Ok(n)
    }
}

impl Seek for StagedInput {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.check()?;
        let offset = self.0.seek(position)?;
        self.check()?;
        Ok(offset)
    }
}

impl StagedInput {
    fn check(&self) -> io::Result<()> {
        if Instant::now() < self.1 {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Games turn timed out.",
            ))
        }
    }
}

pub fn remove(db: &Path, id: &str) {
    if let Ok(path) = path(db, id) {
        let _ = fs::remove_file(path);
    }
}
