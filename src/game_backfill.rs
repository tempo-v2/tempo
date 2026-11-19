//! Explicit, resumable historical Games imports. Provider I/O runs off the Study server.
use crate::adapters::public_games::{FetchedGames, Game, Provider, PublicGames, valid_account};
use crate::game_evidence::{check_id, fingerprint};
use crate::{AppError, GameRange, Tempo};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use tempo_store::GameRecord;
use tempo_types::DatasetId;

const DAY: u64 = 86_400_000;
const WINDOW: u64 = 14 * DAY;
const APPLY_BATCH: usize = 24;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackfillRequest {
    pub command_id: String,
    pub dataset_id: DatasetId,
    pub from_ms: u64,
    pub to_ms: u64,
    pub lichess: Option<String>,
    pub chess_com: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackfillRetry {
    pub dataset_id: DatasetId,
    pub job_id: String,
    pub command_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct BackfillJob {
    pub id: String,
    pub request: BackfillRequest,
    pub resets: BTreeMap<String, u64>,
    pub windows: Vec<Window>,
    retries: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Window {
    source: HistorySource,
    learner: String,
    from_ms: u64,
    to_ms: u64,
    attempt: u32,
    status: WindowStatus,
    received: usize,
    accepted: usize,
    split: bool,
    notes: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum HistorySource {
    Lichess,
    ChessCom,
    Pgn,
}

impl HistorySource {
    fn provider(self) -> Option<Provider> {
        match self {
            Self::Lichess => Some(Provider::Lichess),
            Self::ChessCom => Some(Provider::ChessCom),
            Self::Pgn => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WindowStatus {
    Pending,
    Complete,
    Incomplete,
}

impl Window {
    fn new(source: HistorySource, learner: &str, from_ms: u64, to_ms: u64) -> Self {
        Self {
            source,
            learner: learner.into(),
            from_ms,
            to_ms,
            attempt: 0,
            status: WindowStatus::Pending,
            received: 0,
            accepted: 0,
            split: false,
            notes: vec![],
        }
    }
}

impl BackfillJob {
    pub(crate) fn new(request: BackfillRequest, resets: BTreeMap<String, u64>) -> Self {
        let mut windows = Vec::new();
        let mut add = |source, learner: &str| {
            let mut from = request.from_ms;
            while from < request.to_ms {
                let to = (from + WINDOW).min(request.to_ms);
                windows.push(Window::new(source, learner, from, to));
                from = to;
            }
        };
        for (source, learner) in [
            (HistorySource::Lichess, &request.lichess),
            (HistorySource::ChessCom, &request.chess_com),
        ] {
            if let Some(learner) = learner {
                add(source, learner);
            }
        }
        for learner in [&request.lichess, &request.chess_com]
            .into_iter()
            .flatten()
            .collect::<std::collections::BTreeSet<_>>()
        {
            add(HistorySource::Pgn, learner);
        }
        Self {
            id: request.command_id.clone(),
            request,
            resets,
            windows,
            retries: vec![],
        }
    }

    fn pending(&self) -> Option<usize> {
        self.windows
            .iter()
            .position(|window| window.status == WindowStatus::Pending)
    }

    fn view(&self) -> Value {
        let pending = self
            .windows
            .iter()
            .filter(|w| w.status == WindowStatus::Pending)
            .count();
        let incomplete = self
            .windows
            .iter()
            .filter(|w| w.status == WindowStatus::Incomplete)
            .count();
        serde_json::json!({
            "status": if pending > 0 { "running" } else if incomplete > 0 { "incomplete" } else { "complete" },
            "job_id": self.id, "request": self.request, "windows": self.windows,
            "pending_windows": pending, "incomplete_windows": incomplete,
        })
    }
}

type Request = (Provider, String, GameRange);
type Reply = Result<FetchedGames, AppError>;

struct ApplyWindow {
    index: usize,
    games: WindowGames,
    accepted: usize,
    received: usize,
    notes: Vec<String>,
    incomplete: bool,
    batch: usize,
}

enum WindowGames {
    Provider(Vec<Game>),
    Pgn(Vec<GameRecord>),
}

impl WindowGames {
    fn take_batch(&mut self) -> Self {
        match self {
            Self::Provider(games) => {
                Self::Provider(games.drain(..games.len().min(APPLY_BATCH)).collect())
            }
            Self::Pgn(games) => Self::Pgn(games.drain(..games.len().min(APPLY_BATCH)).collect()),
        }
    }

    fn ids(&self) -> Vec<&str> {
        match self {
            Self::Provider(games) => games.iter().map(|game| game.id.as_str()).collect(),
            Self::Pgn(games) => games.iter().map(|game| game.id.as_str()).collect(),
        }
    }
}

pub struct BackfillCoordinator {
    requests: SyncSender<Request>,
    replies: Receiver<Reply>,
    job: Option<BackfillJob>,
    fetching: Option<usize>,
    applying: Option<ApplyWindow>,
    last_error: Option<String>,
    discard_provider_reply: bool,
}

impl BackfillCoordinator {
    pub fn start(app: &Tempo) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (requests, receive) = sync_channel::<Request>(1);
        let (send, replies) = sync_channel(1);
        std::thread::Builder::new()
            .name("tempo-game-backfill".into())
            .spawn(move || {
                let mut providers = PublicGames::default();
                while let Ok((source, account, range)) = receive.recv() {
                    if send
                        .send(providers.fetch(source, &account, &range))
                        .is_err()
                    {
                        break;
                    }
                }
            })?;
        let job = app.read_backfill(&app.game_dataset()?, None)?;
        Ok(Self {
            requests,
            replies,
            job,
            fetching: None,
            applying: None,
            last_error: None,
            discard_provider_reply: false,
        })
    }

    pub fn status(&self) -> Value {
        let mut view = self
            .job
            .as_ref()
            .map_or_else(|| serde_json::json!({"status":"idle"}), BackfillJob::view);
        if let Some(index) = self
            .fetching
            .or_else(|| self.applying.as_ref().map(|work| work.index))
        {
            view["active_window"] = index.into();
        }
        if let Some(error) = &self.last_error {
            view["status"] = "paused".into();
            view["error"] = error.clone().into();
        }
        view
    }

    pub fn active(&self) -> bool {
        self.last_error.is_none() && self.job.as_ref().is_some_and(|job| job.pending().is_some())
    }

    pub fn begin(
        &mut self,
        app: &mut Tempo,
        mut request: BackfillRequest,
    ) -> Result<Value, AppError> {
        check_id(&request.command_id)?;
        for account in [&mut request.lichess, &mut request.chess_com]
            .into_iter()
            .flatten()
        {
            *account = account.trim().to_ascii_lowercase();
            if !valid_account(account) {
                return Err(invalid("Enter a valid provider account."));
            }
        }
        let now = app.game_now();
        if request.lichess.is_none() && request.chess_com.is_none()
            || request.from_ms % DAY != 0
            || request.to_ms % DAY != 0
            || request.to_ms > now
            || !matches!(
                request.to_ms.checked_sub(request.from_ms),
                Some(1..=31_622_400_000)
            )
        {
            return Err(invalid(
                "Choose accounts and a completed UTC date range of at most 366 days.",
            ));
        }
        if let Some(saved) = app.read_backfill(&request.dataset_id, Some(&request.command_id))? {
            return if request == saved.request {
                Ok(saved.view())
            } else {
                Err(invalid(
                    "The backfill ID already belongs to different input.",
                ))
            };
        }
        if self.job.as_ref().is_some_and(|job| job.pending().is_some()) {
            return Err(invalid(
                "A historical backfill already has pending windows.",
            ));
        }
        let job = app.create_backfill(request)?;
        let view = job.view();
        self.job = Some(job);
        self.last_error = None;
        Ok(view)
    }

    pub fn retry(&mut self, app: &mut Tempo, request: BackfillRetry) -> Result<Value, AppError> {
        check_id(&request.command_id)?;
        check_id(&request.job_id)?;
        if app.game_dataset()? != request.dataset_id {
            return Err(AppError::new(
                "stale_dataset",
                "The active dataset changed.",
            ));
        }

        // The saved acceptance outlives the coordinator's current-job view.
        if let Some(saved) = app.read_backfill(&request.dataset_id, Some(&request.job_id))? {
            if saved.id != request.job_id || saved.request.dataset_id != request.dataset_id {
                return Err(invalid(
                    "The saved historical job has a different identity.",
                ));
            }
            if saved.retries.contains(&request.command_id) {
                return Ok(saved.view());
            }
        }

        let job = self
            .job
            .as_mut()
            .filter(|job| job.id == request.job_id && job.request.dataset_id == request.dataset_id)
            .ok_or_else(|| invalid("The current historical job changed."))?;
        if self.fetching.is_some() || self.applying.is_some() {
            return Err(invalid("A historical window is still being processed."));
        }
        let mut next = job.clone();
        for window in &mut next.windows {
            if window.status == WindowStatus::Incomplete {
                *window = Window {
                    attempt: window.attempt + 1,
                    ..Window::new(window.source, &window.learner, window.from_ms, window.to_ms)
                };
            }
        }
        next.retries.push(request.command_id);
        app.save_backfill(&next, None)?;
        *job = next;
        self.last_error = None;
        Ok(job.view())
    }

    pub fn poll(&mut self, app: &mut Tempo) {
        if self.discard_provider_reply {
            match self.replies.try_recv() {
                Err(TryRecvError::Empty) => return,
                _ => self.discard_provider_reply = false,
            }
        }
        if !self.active() {
            return;
        }
        if let Err(error) = self.advance(app) {
            self.fetching = None;
            self.applying = None;
            self.last_error = Some(error.message);
        }
    }

    fn advance(&mut self, app: &mut Tempo) -> Result<(), AppError> {
        let job = self.job.as_mut().expect("active job");
        if app.game_dataset()? != job.request.dataset_id {
            self.job = app.read_backfill(&app.game_dataset()?, None)?;
            self.applying = None;
            // Keep an outstanding provider read associated with its original job.
            self.discard_provider_reply = self.fetching.take().is_some();
            return Ok(());
        }
        if let Some(mut applying) = self.applying.take() {
            let window = &job.windows[applying.index];
            let batch = applying.games.take_batch();
            let ids = batch.ids();
            if !ids.is_empty() {
                let count = ids.len();
                let key = fingerprint(
                    "historical_batch",
                    &(
                        &job.id,
                        window.source,
                        &window.learner,
                        window.from_ms,
                        window.to_ms,
                        window.attempt,
                        applying.batch,
                        ids,
                    ),
                )?;
                let report = match app.replay_game(&job.request.dataset_id, &key, &key)? {
                    Some(saved) => saved,
                    None => match batch {
                        WindowGames::Provider(games) => app.ingest_provider_games(
                            &job.request.dataset_id,
                            &key,
                            &key,
                            &window.learner,
                            FetchedGames {
                                received: count,
                                games,
                                ..Default::default()
                            },
                            Some(job),
                        )?,
                        WindowGames::Pgn(games) => {
                            app.ingest_historical_pgns(job, &key, &window.learner, games)?
                        }
                    },
                };
                applying.accepted += report.new_games + report.duplicates;
                applying.incomplete |= !report.errors.is_empty();
                applying.notes.extend(report.errors.into_iter().take(10));
                applying.batch += 1;
                self.applying = Some(applying);
                return Ok(());
            }
            let mut next = job.clone();
            let window = &mut next.windows[applying.index];
            window.status = if applying.incomplete {
                WindowStatus::Incomplete
            } else {
                WindowStatus::Complete
            };
            window.received = applying.received;
            window.accepted = applying.accepted;
            window.notes = applying.notes.into_iter().take(20).collect();
            app.save_backfill(&next, None)?;
            *job = next;
            return Ok(());
        }
        if let Some(index) = self.fetching {
            let reply = match self.replies.try_recv() {
                Ok(reply) => reply,
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => {
                    return Err(invalid("The historical provider worker stopped."));
                }
            };
            self.fetching = None;
            let window = &job.windows[index];
            let split = match &reply {
                Ok(batch) => batch.truncated,
                Err(error) => {
                    window.source == HistorySource::Lichess
                        && error.code == "provider_response_limit"
                }
            };
            if split && window.to_ms - window.from_ms > 1 {
                return split_window(app, job, index);
            }
            let mut fetched = reply.unwrap_or_else(|error| FetchedGames {
                errors: vec![error.message],
                ..Default::default()
            });
            fetched.games.sort_by(|a, b| {
                (a.start_ms.unwrap_or(a.end_ms), &a.id)
                    .cmp(&(b.start_ms.unwrap_or(b.end_ms), &b.id))
            });
            let incomplete = fetched.truncated || !fetched.errors.is_empty();
            let mut notes = std::mem::take(&mut fetched.errors);
            notes.extend(
                fetched
                    .unlisted_archives
                    .iter()
                    .map(|month| format!("No Chess.com archive listed for {month}.")),
            );
            self.applying = Some(ApplyWindow {
                index,
                received: fetched.received,
                games: WindowGames::Provider(fetched.games),
                accepted: 0,
                incomplete,
                notes,
                batch: 0,
            });
            return Ok(());
        }
        if let Some(index) = job.pending() {
            let window = &job.windows[index];
            if let Some(provider) = window.source.provider() {
                self.requests
                    .try_send((
                        provider,
                        window.learner.clone(),
                        GameRange {
                            from_ms: window.from_ms,
                            to_ms: window.to_ms,
                        },
                    ))
                    .map_err(|_| invalid("The historical provider worker is unavailable."))?;
                self.fetching = Some(index);
            } else {
                let mut games = app.historical_pgn_window(
                    &job.request.dataset_id,
                    &window.learner,
                    window.from_ms,
                    window.to_ms,
                )?;
                let received = games.len();
                let truncated = received > 500;
                if truncated && window.to_ms - window.from_ms > 1 {
                    return split_window(app, job, index);
                }
                games.truncate(500);
                self.applying = Some(ApplyWindow {
                    index,
                    games: WindowGames::Pgn(games),
                    accepted: 0,
                    received,
                    incomplete: truncated,
                    batch: 0,
                    notes: if truncated {
                        vec![
                            "Stored PGN window exceeds the import limit at one occurrence time."
                                .into(),
                        ]
                    } else {
                        vec![]
                    },
                });
            }
        }
        Ok(())
    }
}

fn split_window(app: &mut Tempo, job: &mut BackfillJob, index: usize) -> Result<(), AppError> {
    let window = &job.windows[index];
    let middle = window.from_ms + (window.to_ms - window.from_ms) / 2;
    let mut next = job.clone();
    next.windows.splice(
        index..=index,
        [
            Window {
                split: true,
                ..Window::new(window.source, &window.learner, window.from_ms, middle)
            },
            Window {
                split: true,
                ..Window::new(window.source, &window.learner, middle, window.to_ms)
            },
        ],
    );
    app.save_backfill(&next, None)?;
    *job = next;
    Ok(())
}

fn invalid(message: &str) -> AppError {
    AppError::new("invalid_backfill", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Components, FixedClock, IntroductionLimits};
    use serde_json::{from_value, json};
    use tempo_chess::{Catalog, LegalSequenceGrader};
    use tempo_learning::{DailyLearning, LearningPolicy};
    use tempo_store::SqliteStore;
    use tempo_types::{StudyDay, StudyTime};

    fn fixture_coordinator(
        app: &Tempo,
    ) -> (BackfillCoordinator, Receiver<Request>, SyncSender<Reply>) {
        let (requests, provider) = sync_channel(1);
        let (response, replies) = sync_channel(1);
        let job = app
            .read_backfill(&app.game_dataset().unwrap(), None)
            .unwrap();
        (
            BackfillCoordinator {
                requests,
                replies,
                job,
                fetching: None,
                applying: None,
                last_error: None,
                discard_provider_reply: false,
            },
            provider,
            response,
        )
    }

    #[test]
    fn retained_retry_acknowledgement_leaves_newer_job_and_windows_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retry.sqlite");
        let dataset = DatasetId::from("dataset");
        let now = 20_000 * DAY;
        let open_app = |store| {
            Tempo::daily(
                Box::new(store),
                Components {
                    clock: Box::new(FixedClock(StudyTime {
                        day: StudyDay(20_000),
                        unix_ms: now,
                    })),
                    learning: Box::new(DailyLearning::default()),
                    ..Components::fixtures()
                },
                IntroductionLimits::default(),
                [42; 32],
                0,
            )
            .unwrap()
        };
        let store = SqliteStore::create(
            &path,
            dataset.clone(),
            Catalog::default(),
            DailyLearning::default().settings(),
        )
        .unwrap();
        let mut app = open_app(store);
        let (mut jobs, provider, response) = fixture_coordinator(&app);
        let begin = |id: &str| BackfillRequest {
            command_id: id.into(),
            dataset_id: dataset.clone(),
            from_ms: now - DAY,
            to_ms: now,
            lichess: Some("learner".into()),
            chess_com: None,
        };
        let pending = r#"{"command_id":"retry-a","dataset_id":"dataset","job_id":"history-a"}"#;
        let exact_retry = || serde_json::from_str::<BackfillRetry>(pending).unwrap();
        let finish_window = |jobs: &mut BackfillCoordinator, app: &mut Tempo, reply: Reply| {
            response.send(reply).unwrap();
            for _ in 0..4 {
                jobs.poll(app);
            }
        };

        jobs.begin(&mut app, begin("history-a")).unwrap();
        jobs.poll(&mut app);
        provider.try_recv().unwrap();
        finish_window(
            &mut jobs,
            &mut app,
            Err(invalid("Fixture provider failure.")),
        );
        assert_eq!(jobs.status()["status"], "incomplete");
        assert_eq!(
            jobs.retry(&mut app, exact_retry()).unwrap()["status"],
            "running"
        );
        jobs.poll(&mut app);
        provider.try_recv().unwrap();
        finish_window(&mut jobs, &mut app, Ok(FetchedGames::default()));

        let assert_replay = |jobs: &mut BackfillCoordinator, app: &mut Tempo, current_id: &str| {
            let before = jobs.status();
            assert_eq!(before["job_id"], current_id);
            let saved_before = app.read_backfill(&dataset, None).unwrap().unwrap();
            assert_eq!(saved_before.id, current_id);
            let reply = jobs.retry(app, exact_retry()).unwrap();
            assert_eq!(reply["job_id"], "history-a");
            assert_eq!(reply["request"]["dataset_id"], "dataset");
            assert_eq!(reply["status"], "complete");
            assert_eq!(jobs.status(), before);
            assert_eq!(
                serde_json::to_vec(&app.read_backfill(&dataset, None).unwrap().unwrap()).unwrap(),
                serde_json::to_vec(&saved_before).unwrap()
            );
            let saved_a = app
                .read_backfill(&dataset, Some("history-a"))
                .unwrap()
                .unwrap();
            assert_eq!(saved_a.retries, ["retry-a"]);
            assert_eq!(saved_a.windows[0].attempt, 1);
        };
        assert_replay(&mut jobs, &mut app, "history-a");

        jobs.begin(&mut app, begin("history-b")).unwrap();
        jobs.poll(&mut app);
        provider.try_recv().unwrap();
        assert!(jobs.status()["active_window"].is_number());
        assert_replay(&mut jobs, &mut app, "history-b");

        let mut unseen_retry = exact_retry();
        unseen_retry.command_id = "unseen-retry".into();
        assert!(jobs.retry(&mut app, unseen_retry).is_err());
        let mut wrong_job = exact_retry();
        wrong_job.job_id = "history-b".into();
        assert!(jobs.retry(&mut app, wrong_job).is_err());
        let mut wrong_dataset = exact_retry();
        wrong_dataset.dataset_id = DatasetId::from("other");
        assert_eq!(
            jobs.retry(&mut app, wrong_dataset).unwrap_err().code,
            "stale_dataset"
        );
        assert_replay(&mut jobs, &mut app, "history-b");

        finish_window(&mut jobs, &mut app, Ok(FetchedGames::default()));
        assert_eq!(jobs.status()["status"], "complete");
        assert_replay(&mut jobs, &mut app, "history-b");
        drop(jobs);
        drop(app);
        let mut reopened = open_app(SqliteStore::open(&path).unwrap());
        let (mut restarted, provider, _) = fixture_coordinator(&reopened);
        assert_replay(&mut restarted, &mut reopened, "history-b");
        assert!(provider.try_recv().is_err());
    }

    #[test]
    fn stored_date_only_backfill_resumes_receipts_and_fences_a_later_same_time_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backfill.sqlite");
        let day = 20_000;
        let now = day * DAY;
        let dataset = DatasetId("dataset".into());
        let learning = DailyLearning::default();
        let store = SqliteStore::create(
            &path,
            dataset.clone(),
            Catalog::default(),
            learning.settings(),
        )
        .unwrap();
        let mut app = Tempo::daily(
            Box::new(store),
            Components {
                clock: Box::new(FixedClock(StudyTime {
                    day: StudyDay(day as i32),
                    unix_ms: now,
                })),
                grader: Box::new(LegalSequenceGrader),
                learning: Box::new(learning),
                ..Components::fixtures()
            },
            IntroductionLimits::default(),
            [42; 32],
            0,
        )
        .unwrap();
        let content = |app: &mut Tempo, value| app.handle_repertoire(from_value(value).unwrap());
        let payload = json!({"command":"preview","dataset_id":dataset,
            "name":"White", "learner_color":"white","pgn":"1. e4 *"});
        let preview = content(&mut app, payload.clone());
        let mut save = payload;
        save["command"] = json!("save");
        save["token"] = preview["token"].clone();
        let saved = content(&mut app, save);
        let repertoire = &saved["repertoires"][0]["id"];
        let reset = |app: &mut Tempo, id| {
            assert_eq!(
                content(
                    app,
                    json!({"command":"reset_progress","command_id":id,
                "dataset_id":dataset,"repertoire_id":repertoire})
                )["status"],
                "progress_reset"
            );
        };
        reset(&mut app, "earlier-reset");
        let pgn = (0..(APPLY_BATCH + 1)).map(|i| format!(
            "[White \"learner\"]\n[Black \"opponent-{i}\"]\n[Date \"2024.10.03\"]\n\n1. d4 d5 0-1\n\n"
        )).collect::<String>();
        let ordinary = app.import_game_pgn(from_value(json!({
            "command_id":"ordinary-import", "dataset_id":dataset, "learner":"learner", "pgn":pgn
        })).unwrap()).unwrap();
        assert_eq!(ordinary.new_games, APPLY_BATCH + 1);
        assert!(app.snapshot().unwrap().assessments.is_empty());

        let request = BackfillRequest {
            command_id: "historical".into(),
            dataset_id: dataset.clone(),
            from_ms: now - DAY,
            to_ms: now,
            lichess: Some("learner".into()),
            chess_com: None,
        };
        assert_eq!(
            app.historical_pgn_window(&dataset, "learner", now - DAY, now)
                .unwrap()
                .len(),
            APPLY_BATCH + 1
        );
        let (mut job, provider, response) = fixture_coordinator(&app);
        assert_eq!(
            job.begin(&mut app, request.clone()).unwrap(),
            job.begin(&mut app, request).unwrap()
        );
        job.poll(&mut app); // The provider is deliberately stalled.
        assert!(provider.try_recv().is_ok());
        assert_eq!(app.study_progress(None).unwrap().unwrap().due, 0);
        assert!(app.snapshot().unwrap().assessments.is_empty());
        response.send(Ok(FetchedGames::default())).unwrap();
        for _ in 0..4 {
            job.poll(&mut app);
        }
        assert!(
            app.snapshot()
                .unwrap()
                .learning
                .values()
                .any(|state| state.correction.is_some()),
            "{}",
            job.status()
        );
        let before_reset = SqliteStore::open(&path).unwrap();
        assert_eq!(
            tempo_store::Store::game_failures(&before_reset, &dataset)
                .unwrap()
                .len(),
            APPLY_BATCH
        );
        assert_eq!(job.status()["pending_windows"], 1);

        // Lose the in-memory window before reporting it, then reset on this same timestamp.
        drop(job);
        reset(&mut app, "later-reset");
        let (mut resumed, provider, response) = fixture_coordinator(&app);
        for _ in 0..6 {
            resumed.poll(&mut app);
        }
        assert_eq!(
            resumed.status()["status"],
            "complete",
            "{}",
            resumed.status()
        );
        assert_eq!(resumed.status()["windows"][1]["accepted"], APPLY_BATCH + 1);
        assert!(
            app.snapshot()
                .unwrap()
                .learning
                .values()
                .all(|state| state.correction.is_none())
        );
        assert_eq!(
            tempo_store::Store::game_failures(&before_reset, &dataset)
                .unwrap()
                .len(),
            APPLY_BATCH
        );

        use tempo_store::Store;
        let item = app
            .snapshot()
            .unwrap()
            .catalog
            .items
            .into_values()
            .next()
            .unwrap();
        let expected = before_reset.game_relevance(&dataset, "").unwrap()[&item.target].exposure;
        app.import_game_pgn(from_value(json!({
            "command_id":"other-pgn", "dataset_id":dataset, "learner":"other",
            "pgn":"[White \"other\"]\n[Black \"opponent\"]\n[Date \"2024.10.03\"]\n\n1. e4 c5 1-0",
        })).unwrap()).unwrap();
        assert_eq!(
            before_reset.game_relevance(&dataset, "").unwrap()[&item.target].exposure,
            expected
        );
        let newer = BackfillRequest {
            command_id: "another-identity".into(),
            dataset_id: dataset.clone(),
            from_ms: now - DAY,
            to_ms: now,
            lichess: None,
            chess_com: Some("other".into()),
        };
        let added = resumed.begin(&mut app, newer.clone()).unwrap();
        assert_eq!(resumed.begin(&mut app, newer.clone()).unwrap(), added);
        let mut conflicting = newer;
        conflicting.chess_com = Some("unrecognized".into());
        assert!(resumed.begin(&mut app, conflicting).is_err());
        assert_eq!(
            before_reset.game_relevance(&dataset, "").unwrap()[&item.target].exposure,
            expected + 1
        );

        resumed.poll(&mut app);
        assert!(provider.try_recv().is_ok());
        response
            .send(Err(AppError::new(
                "fixture_provider",
                "provider unavailable",
            )))
            .unwrap();
        resumed.poll(&mut app);
        resumed.poll(&mut app);
        assert_eq!(resumed.status()["windows"][0]["status"], "incomplete");
        let retried = resumed
            .retry(
                &mut app,
                BackfillRetry {
                    dataset_id: dataset.clone(),
                    job_id: "another-identity".into(),
                    command_id: "retry-other".into(),
                },
            )
            .unwrap();
        assert_eq!(retried["windows"][0]["status"], "pending");
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(
            reopened.game_relevance(&dataset, "").unwrap()[&item.target].exposure,
            expected + 1
        );
    }
}
