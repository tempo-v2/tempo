use serde_json::{Value, json};
use shakmaty::{CastlingMode, Chess, Position, fen::Fen, uci::UciMove};
use std::{
    collections::HashSet,
    env, fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tempo_stockfish_analysis::{Error, Options, Request, Stockfish};
use tokio::time::{sleep, timeout};

const START: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

fn cli(engine: &Path, request: Value) -> (bool, Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tempo-stockfish-analysis"))
        .arg(engine)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    write!(child.stdin.take().unwrap(), "{request}").unwrap();
    let response = child.wait_with_output().unwrap();
    let json = serde_json::from_slice(&response.stdout).unwrap();
    (response.status.success(), json)
}

fn evaluated(engine: &Path, fen: &str, options: Value) -> Value {
    let (success, reply) = cli(engine, json!({ "fen": fen, "options": options }));
    assert!(success, "{reply}");
    assert_eq!(reply["status"], "evaluated");
    reply
}

fn roots(reply: &Value) -> HashSet<&str> {
    let lines = reply["lines"].as_array().unwrap();
    let distinct: HashSet<_> = lines
        .iter()
        .map(|line| line["first_move"].as_str().unwrap())
        .collect();
    assert_eq!(distinct.len(), lines.len());
    distinct
}

#[test]
#[ignore = "needs a host-provisioned Stockfish 19 binary"]
fn native_cli_covers_legal_moves_and_white_relative_scores() {
    let engine =
        PathBuf::from(env::var_os("TEMPO_TEST_STOCKFISH").expect("set TEMPO_TEST_STOCKFISH"));
    let (success, full) = cli(&engine, json!({ "fen": START }));
    assert!(success, "{full}");
    assert_eq!(full["depth"], 10);
    assert_eq!(full["engine"], "Stockfish 19");
    assert_eq!(roots(&full).len(), 20);

    let complex = "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";
    let board: Chess = complex
        .parse::<Fen>()
        .unwrap()
        .into_position(CastlingMode::Standard)
        .unwrap();
    let legal: HashSet<_> = board
        .legal_moves()
        .into_iter()
        .map(|play| UciMove::from_move(play, CastlingMode::Standard).to_string())
        .collect();
    assert_eq!(legal.len(), 48);
    let reply = evaluated(&engine, complex, json!({ "depth": 8 }));
    assert_eq!(roots(&reply), legal.iter().map(String::as_str).collect());

    let selected = evaluated(
        &engine,
        START,
        json!({ "coverage": { "kind": "moves", "moves": ["e2e4", "f2f3"] } }),
    );
    assert_eq!(roots(&selected), HashSet::from(["e2e4", "f2f3"]));
    assert_eq!(selected["lines"][0]["first_move"], "e2e4");

    let black = "6k1/5ppp/8/8/8/8/5PPP/3Q2K1 b - - 0 1";
    let reply = evaluated(
        &engine,
        black,
        json!({ "coverage": { "kind": "top", "count": 2 } }),
    );
    let scores: Vec<_> = reply["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line["score"]["white"].as_i64().unwrap())
        .collect();
    assert_eq!(scores.len(), 2);
    assert!(scores[0] > 0 && scores[0] <= scores[1], "{scores:?}");

    let black_mates = "8/8/8/8/8/5kq1/8/7K b - - 0 1";
    let mate = evaluated(
        &engine,
        black_mates,
        json!({ "coverage": { "kind": "top", "count": 1 } }),
    );
    assert_eq!(
        mate["lines"][0]["score"],
        json!({ "kind": "mate", "winner": "black", "moves": 1 })
    );

    let promotion = "7k/P7/8/8/8/8/8/7K w - - 0 1";
    let reply = evaluated(&engine, promotion, json!({ "depth": 5 }));
    let moves = roots(&reply);
    for suffix in ['q', 'r', 'b', 'n'] {
        assert!(moves.contains(format!("a7a8{suffix}").as_str()), "{reply}");
    }
}

#[test]
fn terminal_and_bad_input_resolve_before_launch() {
    let missing = Path::new("/nonexistent/stockfish");
    let (ok, reply) = cli(missing, json!({ "fen": "7k/6Q1/5K2/8/8/8/8/8 b - - 0 1" }));
    assert!(ok);
    assert_eq!(
        reply,
        json!({ "status": "terminal", "outcome": { "kind": "checkmate", "winner": "white" } })
    );
    let (ok, reply) = cli(missing, json!({ "fen": "7k/5K2/6Q1/8/8/8/8/8 b - - 0 1" }));
    assert!(ok);
    assert_eq!(reply["outcome"]["kind"], "stalemate");

    for input in [
        json!({ "fen": "" }),
        json!({ "fen": format!("{START}\nquit") }),
        json!({ "fen": START.replace("KQkq", "HAha") }),
        json!({ "fen": START.replace("KQkq", "KKKK") }),
        json!({ "fen": START.replace("/8/", "/44/") }),
        json!({ "fen": START.replace("0 1", "0 0") }),
        json!({ "fen": "8/8/8/8/8/2k5/8/K7[Pn] w - - 0 1" }),
        json!({ "fen": "8/8/8/8/8/2k5/8/K7 w - - 1+2 0" }),
        json!({ "fen": START.replace("0 1", "32768 1") }),
        json!({ "fen": START.replace("0 1", "0 100001") }),
        json!({ "fen": START, "options": { "coverage": { "kind": "all", "moves": ["e2e4"] } } }),
        json!({ "fen": START, "options": { "depth": 19 } }),
        json!({ "fen": START, "options": { "coverage": { "kind": "moves", "moves": ["e2e4", "e2e4"] } } }),
        json!({ "fen": START, "options": { "coverage": { "kind": "moves", "moves": ["e2e5"] } } }),
    ] {
        let (ok, reply) = cli(missing, input);
        assert!(!ok);
        assert_eq!(reply["error"]["kind"], "invalid_request");
    }
    let (ok, reply) = cli(missing, json!({ "fen": START }));
    assert!(!ok);
    assert_eq!(reply["error"]["kind"], "engine_unavailable");
}

#[tokio::test]
async fn contention_and_cancellation_keep_one_physical_slot() {
    let fixture = Fixture::new("exec /bin/sleep 30");
    let stockfish = fixture.stockfish();
    let (stop, cancelled) = tokio::sync::oneshot::channel();
    let service = stockfish.clone();
    let work = tokio::spawn(async move {
        service
            .analyze_until(start(), async {
                let _ = cancelled.await;
            })
            .await
    });
    let pid = fixture.pid().await;
    assert_eq!(stockfish.analyze(start()).await, Err(Error::Busy));
    stop.send(()).unwrap();
    assert_eq!(work.await.unwrap(), Err(Error::Cancelled));
    assert_dead(&pid);

    fs::remove_file(fixture.path.join("pid")).unwrap();
    let service = stockfish.clone();
    let work = tokio::spawn(async move { service.analyze(start()).await });
    let pid = fixture.pid().await;
    work.abort();
    timeout_reap(&pid).await;
    assert_dead(&pid);
}

#[test]
fn caller_runtime_shutdown_preserves_slot_through_reaping() {
    use std::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };
    use tokio::runtime::Builder;

    let fixture = Fixture::new("exec /bin/sleep 30");
    let stockfish = fixture.stockfish();
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let old_pid = runtime.block_on(async {
        let service = stockfish.clone();
        tokio::spawn(async move { service.analyze(start()).await });
        fixture.pid().await
    });
    drop(runtime);

    let replacement = Builder::new_current_thread().enable_all().build().unwrap();
    let (stop, cancelled) = tokio::sync::oneshot::channel();
    let mut work = pin!(stockfish.analyze_until(start(), async {
        let _ = cancelled.await;
    }));
    let admission = {
        let _enter = replacement.enter();
        work.as_mut().poll(&mut Context::from_waker(Waker::noop()))
    };
    match admission {
        Poll::Pending => {
            assert_dead(&old_pid);
            stop.send(()).unwrap();
            assert_eq!(replacement.block_on(work), Err(Error::Cancelled));
        }
        Poll::Ready(result) => assert_eq!(result, Err(Error::Busy)),
    }
    replacement.block_on(timeout_reap(&old_pid));
}

async fn timeout_reap(pid: &str) {
    timeout(Duration::from_secs(2), async {
        while is_alive(pid) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn timeout_reaps_the_engine() {
    let fixture = Fixture::new("exec /bin/sleep 30");
    let stockfish = fixture.stockfish();
    let result = timeout(Duration::from_secs(7), stockfish.analyze(start()))
        .await
        .unwrap();
    assert_eq!(result, Err(Error::Timeout));
    assert_dead(&fixture.pid().await);
}

#[tokio::test]
async fn malformed_and_incomplete_output_fail_closed() {
    let bad = Fixture::new("printf 'uciok\\n'; exec /bin/sleep 30");
    assert_eq!(bad.stockfish().analyze(start()).await, Err(Error::Protocol));
    assert_dead(&bad.pid().await);

    let engine = Fixture::new(
        r#"printf 'id name Stockfish fixture\nuciok\n'
while IFS= read -r line; do
    case "$line" in
        isready) printf 'readyok\n' ;;
        go*) printf 'info depth 10 multipv 1 score cp 25 pv e2e4\nbestmove e2e4\n' ;;
    esac
done"#,
    );
    assert_eq!(
        engine.stockfish().analyze(start()).await,
        Err(Error::Incomplete)
    );
    assert_dead(&engine.pid().await);
}

#[test]
fn search_progress_does_not_interrupt_scored_lines() {
    let engine = Fixture::new(
        r#"printf 'id name Stockfish fixture
uciok
'
while IFS= read -r line; do
    case "$line" in
        isready) printf 'readyok
' ;;
        go*) printf 'info depth 10 currmove e2e4 currmovenumber 1
info depth 10 multipv 1 score cp 25 pv e2e4
bestmove e2e4
' ;;
    esac
done"#,
    );
    let reply = evaluated(
        &engine.path.join("engine"),
        START,
        json!({
            "coverage": { "kind": "top", "count": 1 }
        }),
    );
    assert_eq!(roots(&reply), HashSet::from(["e2e4"]));
    assert_eq!(reply["lines"][0]["score"]["white"], 25);
}

fn start() -> Request {
    Request {
        fen: START.to_string(),
        options: Options::default(),
    }
}

fn is_alive(pid: &str) -> bool {
    Command::new("/bin/kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn assert_dead(pid: &str) {
    assert!(!is_alive(pid), "engine {pid} survived its request");
}

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(body: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = env::temp_dir().join(format!(
            "tempo-stockfish-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let executable = path.join("engine");
        fs::write(
            &executable,
            format!("#!/bin/sh\nprintf '%s\\n' \"$$\" > \"${{0%/*}}/pid\"\n{body}\n"),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        Self { path }
    }

    fn stockfish(&self) -> Stockfish {
        Stockfish::new(self.path.join("engine"))
    }

    async fn pid(&self) -> String {
        timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(pid) = fs::read_to_string(self.path.join("pid"))
                    && let Ok(pid) = pid.trim().parse::<u32>()
                {
                    break pid.to_string();
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
