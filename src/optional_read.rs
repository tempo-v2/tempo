use crate::db::Database;
use anyhow::{Result, ensure};
use rusqlite::{Connection, ErrorCode};
use serde::Serialize;
use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

const DEADLINE: Duration = Duration::from_millis(250);
const VM_BUDGET: u32 = 2_000_000;
const CLOCK_CHECK_INTERVAL: u32 = 1_000;
const MAX_JSON: usize = 2 * 1024 * 1024;
#[derive(Debug, Clone, Copy)]
pub struct Unavailable;

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("This view is temporarily unavailable.")
    }
}
impl std::error::Error for Unavailable {}

#[derive(Clone, Copy)]
pub enum Reader {
    Counts,
    StudyContext,
    GamesStatus,
    Repertoires,
    GamesArchive,
    GameReplay,
    Openings,
    Tactics,
    ProviderSettings,
    OpeningSettings,
    CurrentStudy,
}

impl Reader {
    fn slot(self) -> &'static Semaphore {
        const LANES: usize = Reader::CurrentStudy as usize + 1;
        static SLOTS: [Semaphore; LANES] = [const { Semaphore::const_new(1) }; LANES];
        &SLOTS[self as usize]
    }

    fn hides_read_errors(self) -> bool {
        matches!(
            self,
            Self::Counts | Self::StudyContext | Self::GamesStatus | Self::Repertoires
        )
    }
}

pub async fn read<T: Serialize + 'static>(
    reader: Reader,
    database: PathBuf,
    work: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
) -> Result<Vec<u8>> {
    run(reader.slot(), DEADLINE, move |deadline, cancelled| {
        let mut db =
            Database::read_with_busy_timeout(&database, Duration::ZERO).map_err(|_| Unavailable)?;
        limit_query(&db, deadline, VM_BUDGET, cancelled.clone());
        let value = work(&mut db).map_err(|error| {
            if reader.hides_read_errors()
                || error.chain().any(|cause| {
                    matches!(cause.downcast_ref::<rusqlite::Error>(),
                    Some(rusqlite::Error::SqliteFailure(detail, _)) if matches!(
                        detail.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked |
                        ErrorCode::OperationInterrupted))
                })
            {
                Unavailable.into()
            } else {
                error
            }
        })?;
        let mut output = JsonBody {
            bytes: Vec::new(),
            deadline,
            cancelled,
        };
        serde_json::to_writer(&mut output, &value).map_err(|_| Unavailable)?;
        ensure!(Instant::now() < deadline, Unavailable);
        Ok(output.bytes)
    })
    .await
}

struct JsonBody {
    bytes: Vec<u8>,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl Write for JsonBody {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_JSON - self.bytes.len()
            || Instant::now() >= self.deadline
            || self.cancelled.load(Ordering::Relaxed)
        {
            return Err(io::Error::other(Unavailable));
        }
        self.bytes.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct CancelRead {
    cancelled: Arc<AtomicBool>,
    task: tokio::task::AbortHandle,
}

impl Drop for CancelRead {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.task.abort();
    }
}

async fn run<T: Send + 'static>(
    slot: &'static Semaphore,
    timeout: Duration,
    work: impl FnOnce(Instant, Arc<AtomicBool>) -> Result<T> + Send + 'static,
) -> Result<T> {
    let deadline = Instant::now() + timeout;
    let permit = slot.try_acquire().map_err(|_| Unavailable)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    let mut task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        ensure!(
            Instant::now() < deadline && !worker_cancelled.load(Ordering::Relaxed),
            Unavailable
        );
        work(deadline, worker_cancelled)
    });
    let _cancel = CancelRead {
        cancelled,
        task: task.abort_handle(),
    };
    tokio::time::timeout_at(deadline.into(), &mut task)
        .await
        .map_err(|_| Unavailable)?
        .map_err(|_| Unavailable)?
}

fn limit_query(db: &Connection, deadline: Instant, budget: u32, cancelled: Arc<AtomicBool>) {
    let mut steps = 0;
    db.progress_handler(
        1,
        Some(move || {
            steps += 1;
            steps >= budget
                || (steps.is_multiple_of(CLOCK_CHECK_INTERVAL)
                    && (Instant::now() >= deadline || cancelled.load(Ordering::Relaxed)))
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[test]
    fn statement_work_accumulates_across_queries() -> Result<()> {
        let db = Connection::open_in_memory()?;
        limit_query(
            &db,
            Instant::now() + Duration::from_secs(2),
            8_000,
            Arc::default(),
        );
        let select = "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<260)
                      SELECT sum(i) FROM n";
        assert_eq!(
            db.query_row(select, [], |row| row.get::<_, i64>(0))?,
            33_930
        );
        assert!(matches!(
            db.query_row(select, [], |row| row.get::<_, i64>(0)),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::OperationInterrupted
        ));
        // Even work below a coarse progress interval contributes to the shared budget.
        limit_query(
            &db,
            Instant::now() + Duration::from_secs(2),
            100,
            Arc::default(),
        );
        let completed = (0..100)
            .take_while(|_| db.execute_batch("SELECT 1").is_ok())
            .count();
        assert!(completed > 0 && completed < 100);
        Ok(())
    }

    #[tokio::test]
    async fn timed_out_work_keeps_slot_until_worker_exits() {
        static TEST_SLOT: Semaphore = Semaphore::const_new(1);
        let (started, running) = oneshot::channel();
        let (release, held) = oneshot::channel();
        let task = tokio::spawn(run(&TEST_SLOT, DEADLINE, move |_, _| {
            started.send(()).unwrap();
            held.blocking_recv().unwrap();
            Ok(())
        }));
        running.await.unwrap();
        assert!(task.await.unwrap().unwrap_err().is::<Unavailable>());
        assert!(
            run(&TEST_SLOT, DEADLINE, |_, _| Ok(()))
                .await
                .unwrap_err()
                .is::<Unavailable>()
        );
        release.send(()).unwrap();
        let available = tokio::time::timeout(Duration::from_secs(1), TEST_SLOT.acquire())
            .await
            .unwrap()
            .unwrap();
        drop(available);
        run(&TEST_SLOT, DEADLINE, |_, _| Ok(())).await.unwrap();
    }

    #[test]
    fn response_budget_keeps_partial_json_private() -> Result<()> {
        let mut body = JsonBody {
            bytes: Vec::new(),
            deadline: Instant::now() + Duration::from_secs(2),
            cancelled: Arc::default(),
        };
        body.write_all(&vec![0; MAX_JSON])?;
        assert!(body.write_all(b"extra").is_err());
        assert_eq!(body.bytes.len(), MAX_JSON);
        Ok(())
    }

    #[tokio::test]
    async fn dropped_caller_does_not_release_a_running_read() {
        let slot = Reader::GamesArchive.slot();
        let (started, running) = oneshot::channel();
        let (release, held) = oneshot::channel();
        let call = tokio::spawn(run(slot, DEADLINE, move |_, cancelled| {
            started.send(()).unwrap();
            held.blocking_recv().unwrap();
            assert!(cancelled.load(Ordering::Relaxed));
            Ok(())
        }));
        running.await.unwrap();
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
        assert!(slot.try_acquire().is_err());
        run(Reader::CurrentStudy.slot(), DEADLINE, |_, _| Ok(()))
            .await
            .unwrap();
        release.send(()).unwrap();
        let _available = tokio::time::timeout(Duration::from_secs(1), slot.acquire())
            .await
            .unwrap()
            .unwrap();
    }
}
