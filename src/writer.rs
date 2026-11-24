use anyhow::{Result, ensure};
use rusqlite::Connection;
use std::{
    collections::VecDeque,
    fs::File,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

const WAITING: usize = 8;
const WAIT_LIMIT: Duration = Duration::from_secs(2);
const FOREGROUND_TURNS: usize = 4;

#[derive(Debug)]
pub struct Unavailable;

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tempo's writer is busy. Retry the same operation.")
    }
}
impl std::error::Error for Unavailable {}

type Apply = Box<dyn FnOnce(&mut Connection, bool) + Send>;
const WAITING_FOR_CLAIM: u8 = 0;
const CLAIMED: u8 = 1;
const EXPIRED: u8 = 2;

pub struct Admission<T> {
    deadline: Instant,
    claim: Arc<AtomicU8>,
    reply: oneshot::Receiver<Result<T>>,
}

impl<T> Admission<T> {
    pub async fn wait(mut self) -> Result<T> {
        match tokio::time::timeout_at(self.deadline.into(), &mut self.reply).await {
            Ok(reply) => reply?,
            Err(_)
                if self
                    .claim
                    .compare_exchange(
                        WAITING_FOR_CLAIM,
                        EXPIRED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok() =>
            {
                Err(Unavailable.into())
            }
            Err(_) => self.reply.await?,
        }
    }
}

struct Proposal {
    deadline: Instant,
    claim: Arc<AtomicU8>,
    apply: Apply,
}
#[derive(Default)]
struct Waiting {
    foreground: VecDeque<Proposal>,
    background: VecDeque<Proposal>,
    foreground_turns: usize,
}
type Mailbox = (Mutex<Waiting>, Condvar);

#[derive(Clone)]
pub struct Writer(Arc<Mailbox>);

impl Writer {
    pub fn start(mut db: Connection, ownership: Option<File>) -> Result<Self> {
        let mailbox = Arc::new((Mutex::new(Waiting::default()), Condvar::new()));
        let worker = mailbox.clone();
        std::thread::Builder::new()
            .name("tempo-writer".into())
            .spawn(move || {
                let _ownership = ownership;
                let (lock, ready) = &*worker;
                loop {
                    let proposal = {
                        let mut state = ready
                            .wait_while(lock.lock().unwrap(), |state| {
                                state.foreground.is_empty() && state.background.is_empty()
                            })
                            .unwrap();
                        if !state.background.is_empty()
                            && (state.foreground.is_empty()
                                || state.foreground_turns >= FOREGROUND_TURNS)
                        {
                            state.foreground_turns = 0;
                            state.background.pop_front().unwrap()
                        } else {
                            state.foreground_turns =
                                (state.foreground_turns + 1).min(FOREGROUND_TURNS);
                            state.foreground.pop_front().unwrap()
                        }
                    };
                    let claimed = Instant::now() < proposal.deadline
                        && proposal
                            .claim
                            .compare_exchange(
                                WAITING_FOR_CLAIM,
                                CLAIMED,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_ok();
                    (proposal.apply)(&mut db, claimed);
                }
            })?;
        Ok(Self(mailbox))
    }

    pub fn submit<T: Send + 'static>(
        &self,
        background: bool,
        apply: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<Admission<T>> {
        let (send, receive) = oneshot::channel();
        let deadline = Instant::now() + WAIT_LIMIT;
        let (lock, ready) = &*self.0;
        let mut state = lock.lock().unwrap();
        state
            .foreground
            .retain(|proposal| proposal.claim.load(Ordering::Acquire) != EXPIRED);
        ensure!(
            state.foreground.len() + state.background.len() < WAITING,
            Unavailable
        );
        ensure!(
            background || state.foreground.len() < WAITING - 1,
            Unavailable
        );
        let claim = Arc::new(AtomicU8::new(WAITING_FOR_CLAIM));
        let proposal = Proposal {
            deadline,
            claim: claim.clone(),
            apply: Box::new(move |db, timely| {
                let result = if timely {
                    apply(db)
                } else {
                    Err(Unavailable.into())
                };
                let _ = send.send(result);
            }),
        };
        if background {
            state.background.push_back(proposal);
        } else {
            state.foreground.push_back(proposal);
        }
        ready.notify_one();
        Ok(Admission {
            deadline,
            claim,
            reply: receive,
        })
    }

    pub fn background<T: Send + 'static>(
        &self,
        mut apply: impl FnMut(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        // Admission retries are safe. Once a proposal starts, it is never retried here.
        let apply = Arc::new(Mutex::new(move |db: &mut Connection| apply(db)));
        loop {
            let work = apply.clone();
            let outcome = match self.submit(true, move |db| (work.lock().unwrap())(db)) {
                Ok(reply) => reply.reply.blocking_recv()?,
                Err(error) => Err(error),
            };
            match outcome {
                Err(error) if error.is::<Unavailable>() => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                result => return result,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_waiting_work_and_reserves_a_background_turn() -> Result<()> {
        let writer = Writer::start(Connection::open_in_memory()?, None)?;
        let (opened, entered) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let in_flight = writer.submit(false, move |db| {
            db.execute("CREATE TABLE turns (tag TEXT)", [])?;
            opened.send(())?;
            gate.recv()?;
            Ok(())
        })?;
        entered.recv()?;
        let mut waiting = Vec::new();
        for _ in 0..7 {
            waiting.push(writer.submit(false, |db| {
                db.execute("INSERT INTO turns VALUES ('foreground')", [])?;
                Ok(())
            })?);
        }
        assert!(writer.submit(false, |_| Ok(())).is_err());
        let background = writer.submit(true, |db| {
            Ok(db.query_row("SELECT count(*) FROM turns", [], |row| {
                row.get::<_, usize>(0)
            })?)
        })?;
        assert!(writer.submit(true, |_| Ok(())).is_err());
        release.send(())?;
        in_flight.reply.blocking_recv()??;
        assert_eq!(background.reply.blocking_recv()??, 3);
        for turn in waiting {
            turn.reply.blocking_recv()??;
        }
        Ok(())
    }
}
