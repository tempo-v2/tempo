use crate::db::Database;
use anyhow::{Result, ensure};
use rusqlite::Connection;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

const DEADLINE: Duration = Duration::from_millis(250);
const VM_BUDGET: u32 = 2_000_000;
const CLOCK_CHECK_INTERVAL: u32 = 1_000;
#[derive(Debug, Clone, Copy)]
pub struct Unavailable;

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("This view is temporarily unavailable.")
    }
}
impl std::error::Error for Unavailable {}

pub enum Reader {
    Counts,
    StudyContext,
    GamesStatus,
    Repertoires,
}

pub async fn read<T: Send + 'static>(
    reader: Reader,
    database: PathBuf,
    work: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
) -> Result<T, Unavailable> {
    static COUNTS: Semaphore = Semaphore::const_new(1);
    static STUDY_CONTEXT: Semaphore = Semaphore::const_new(1);
    static GAMES_STATUS: Semaphore = Semaphore::const_new(1);
    static REPERTOIRES: Semaphore = Semaphore::const_new(1);
    let slot = match reader {
        Reader::Counts => &COUNTS,
        Reader::StudyContext => &STUDY_CONTEXT,
        Reader::GamesStatus => &GAMES_STATUS,
        Reader::Repertoires => &REPERTOIRES,
    };
    run(slot, DEADLINE, move |deadline| {
        let mut db = Database::read_with_busy_timeout(&database, Duration::ZERO)?;
        limit_query(&db, deadline, VM_BUDGET);
        let result = work(&mut db)?;
        ensure!(Instant::now() < deadline, "Optional read expired");
        Ok(result)
    })
    .await
    .ok_or(Unavailable)
}

async fn run<T: Send + 'static>(
    slot: &'static Semaphore,
    timeout: Duration,
    work: impl FnOnce(Instant) -> Result<T> + Send + 'static,
) -> Option<T> {
    let deadline = Instant::now() + timeout;
    let permit = slot.try_acquire().ok()?;
    let mut task = tokio::task::spawn_blocking(move || {
        // A disconnected or timed-out caller cannot release the physical slot.
        let _permit = permit;
        if Instant::now() >= deadline {
            return None;
        }
        work(deadline).ok()
    });
    match tokio::time::timeout_at(deadline.into(), &mut task).await {
        Ok(result) => result.ok().flatten(),
        Err(_) => {
            // Abort can remove a blocking task that has not started yet.
            task.abort();
            None
        }
    }
}

fn limit_query(db: &Connection, deadline: Instant, budget: u32) {
    let mut steps = 0;
    db.progress_handler(
        1,
        Some(move || {
            steps += 1;
            steps >= budget
                || (steps.is_multiple_of(CLOCK_CHECK_INTERVAL) && Instant::now() >= deadline)
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
        limit_query(&db, Instant::now() + Duration::from_secs(2), 8_000);
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
        limit_query(&db, Instant::now() + Duration::from_secs(2), 100);
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
        let task = tokio::spawn(run(&TEST_SLOT, DEADLINE, move |_| {
            started.send(()).unwrap();
            held.blocking_recv().unwrap();
            Ok(())
        }));
        running.await.unwrap();
        assert_eq!(task.await.unwrap(), None);
        assert_eq!(run(&TEST_SLOT, DEADLINE, |_| Ok(())).await, None);
        release.send(()).unwrap();
        let available = tokio::time::timeout(Duration::from_secs(1), TEST_SLOT.acquire())
            .await
            .unwrap()
            .unwrap();
        drop(available);
        assert_eq!(run(&TEST_SLOT, DEADLINE, |_| Ok(())).await, Some(()));
    }
}
