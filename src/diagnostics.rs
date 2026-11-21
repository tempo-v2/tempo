//! Bounded, local, observational runtime diagnostics.
//!
//! The host owns files and resource sampling. Store uses `log` and has no host
//! dependency. Request paths, SQL, arguments, content and arbitrary error text
//! are never recorded. A full queue or failed writer drops diagnostics, never
//! application work. Durations use a monotonic clock; wall time joins processes.
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const CAPACITY: usize = 1024;
const MAX_ACTIVE: usize = 256;
const MAX_BYTES: u64 = 5 * 1024 * 1024;
const SLOW_MS: u64 = 250;
static RECORDER: OnceLock<Recorder> = OnceLock::new();
static SEQUENCE: AtomicU64 = AtomicU64::new(1);
static STORE_LOGGER: StoreLogger = StoreLogger;
thread_local! {
    static REQUEST: RefCell<Option<String>> = const { RefCell::new(None) };
    static STACK: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    static STORE_STACK: RefCell<Vec<(u64, Arc<AtomicBool>)>> = const { RefCell::new(Vec::new()) };
}

struct Recorder {
    sender: mpsc::SyncSender<Value>,
    run: String,
    start: Instant,
    debug: bool,
    dropped: AtomicU64,
    active: Mutex<BTreeMap<u64, Active>>,
    stopped: Arc<AtomicBool>,
}

impl Recorder {
    fn enqueue(&self, event: Value) {
        if self.sender.try_send(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Clone)]
struct Active {
    started: Instant,
    fields: Map<String, Value>,
    live: Arc<AtomicBool>,
}

/// Holds runtime diagnostics for one serving process. Dropping never waits on I/O.
pub struct DiagnosticsGuard {
    stop: Option<Arc<AtomicBool>>,
}
impl Drop for DiagnosticsGuard {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

/// Enable local JSONL logs before opening the database. Failure is observational.
pub fn init(database: &Path) -> DiagnosticsGuard {
    if RECORDER.get().is_some() || std::env::var("TEMPO_LOG").is_ok_and(|v| v == "off") {
        return DiagnosticsGuard { stop: None };
    }
    let directory = std::env::var_os("TEMPO_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            database
                .parent()
                .unwrap_or(Path::new("."))
                .join("diagnostics")
        });
    let mut nonce = [0u8; 8];
    let _ = getrandom::getrandom(&mut nonce);
    let run = format!(
        "{:x}-{}-{:x}",
        wall_ms(),
        std::process::id(),
        u64::from_le_bytes(nonce)
    );
    let (sender, receiver) = mpsc::sync_channel(CAPACITY);
    let stop = Arc::new(AtomicBool::new(false));
    let recorder = Recorder {
        sender,
        run: run.clone(),
        start: Instant::now(),
        debug: std::env::var("TEMPO_LOG").is_ok_and(|v| v == "debug" || v == "trace"),
        dropped: AtomicU64::new(0),
        active: Mutex::new(BTreeMap::new()),
        stopped: stop.clone(),
    };
    if RECORDER.set(recorder).is_err() {
        return DiagnosticsGuard { stop: None };
    }
    if log::set_logger(&STORE_LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
    let writer_stop = stop.clone();
    if thread::Builder::new()
        .name("tempo-log-writer".into())
        .spawn(move || {
            let mut writer = match RotatingWriter::new(&directory, &run, MAX_BYTES) {
                Ok(writer) => writer,
                Err(_) => {
                    eprintln!(
                        "Tempo diagnostics: log directory unavailable; application continues."
                    );
                    return;
                }
            };
            loop {
                match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(value) => {
                        if writer.record(&value).is_err() {
                            eprintln!(
                                "Tempo diagnostics: writer unavailable; application continues."
                            );
                            return;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout)
                        if !writer_stop.load(Ordering::Relaxed) => {}
                    Err(_) => break,
                }
            }
        })
        .is_err()
    {
        eprintln!("Tempo diagnostics: writer thread unavailable; application continues.");
    }
    let monitor_stop = stop.clone();
    let _ = thread::Builder::new()
        .name("tempo-watchdog".into())
        .spawn(move || monitor(monitor_stop));
    emit(
        "runtime.start",
        json!({"operation":"runtime.serve", "outcome":"started", "cpu_count": cpu_count()}),
    );
    DiagnosticsGuard { stop: Some(stop) }
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
fn millis(start: Instant) -> u64 {
    start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}
fn cpu_count() -> usize {
    thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}

fn identifier(value: &str) -> bool {
    (8..=64).contains(&value.len()) && value.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-')
}

/// Correlation describes one delivery, never the saved command's identity.
pub struct RequestContext {
    previous: Option<String>,
    id: String,
    _thread: PhantomData<Rc<()>>,
}
impl RequestContext {
    pub fn new(incoming: Option<&str>) -> Self {
        let id = incoming
            .filter(|value| identifier(value))
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let mut bytes = [0u8; 16];
                if getrandom::getrandom(&mut bytes).is_ok() {
                    bytes.iter().map(|b| format!("{b:02x}")).collect()
                } else {
                    format!(
                        "{:016x}{:016x}",
                        wall_ms() ^ u64::from(std::process::id()),
                        SEQUENCE.fetch_add(1, Ordering::Relaxed)
                    )
                }
            });
        let previous = REQUEST.with(|value| value.replace(Some(id.clone())));
        Self {
            previous,
            id,
            _thread: PhantomData,
        }
    }
    pub fn id(&self) -> &str {
        &self.id
    }
}
impl Drop for RequestContext {
    fn drop(&mut self) {
        REQUEST.with(|value| {
            value.replace(self.previous.take());
        });
    }
}
pub fn current_request_id() -> Option<String> {
    REQUEST.with(|value| value.borrow().clone())
}
pub fn capture_context() -> Option<String> {
    current_request_id()
}
pub fn run_id() -> Option<String> {
    RECORDER.get().map(|r| r.run.clone())
}
pub fn with_context<T>(context: Option<String>, execute: impl FnOnce() -> T) -> T {
    let previous = REQUEST.with(|value| value.replace(context.filter(|value| identifier(value))));
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            REQUEST.with(|v| {
                v.replace(self.0.take());
            });
        }
    }
    let _restore = Restore(previous);
    execute()
}

/// A host operation's lifetime, including unsuccessful early returns and unwinding.
pub struct Span {
    id: u64,
    start: Instant,
    fields: Map<String, Value>,
    live: Arc<AtomicBool>,
    enabled: bool,
    _thread: PhantomData<Rc<()>>,
}
impl Span {
    pub fn new(operation: &'static str) -> Self {
        let start = Instant::now();
        let id = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let live = Arc::new(AtomicBool::new(true));
        let enabled = RECORDER
            .get()
            .is_some_and(|r| !r.stopped.load(Ordering::Relaxed));
        let mut fields = Map::new();
        if enabled {
            fields.insert("operation".into(), json!(operation));
            fields.insert("span_id".into(), json!(id));
            STACK.with(|stack| {
                let mut stack = stack.borrow_mut();
                if let Some(parent) = stack.last() {
                    fields.insert("parent_span_id".into(), json!(parent));
                }
                stack.push(id);
            });
            if let Some(request) = current_request_id() {
                fields.insert("request_id".into(), json!(request));
            }
            register(
                id,
                Active {
                    started: start,
                    fields: fields.clone(),
                    live: live.clone(),
                },
            );
            if operation == "http.request" || RECORDER.get().is_some_and(|r| r.debug) {
                emit("span.start", Value::Object(fields.clone()));
            }
        }
        Self {
            id,
            start,
            fields,
            live,
            enabled,
            _thread: PhantomData,
        }
    }
    pub fn field(&mut self, key: &'static str, value: impl Serialize) {
        if !self.enabled {
            return;
        }
        if let Ok(value) = serde_json::to_value(value) {
            if valid_field(key, &value) {
                self.fields.insert(key.into(), value);
                if let Some(recorder) = RECORDER.get() {
                    if let Ok(mut active) = recorder.active.try_lock() {
                        if let Some(span) = active.get_mut(&self.id) {
                            span.fields = self.fields.clone();
                        }
                    }
                }
            }
        }
    }
    pub fn set_outcome(&mut self, outcome: &'static str) {
        self.field("outcome", outcome);
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        if !self.enabled {
            return;
        }
        self.live.store(false, Ordering::Relaxed);
        STACK.with(|stack| stack.borrow_mut().retain(|id| *id != self.id));
        unregister(self.id);
        let elapsed = millis(self.start);
        self.fields.insert("elapsed_ms".into(), json!(elapsed));
        self.fields.entry("outcome").or_insert_with(|| {
            json!(if thread::panicking() {
                "unwinding"
            } else {
                "returned"
            })
        });
        if elapsed >= SLOW_MS {
            self.fields.insert("level".into(), json!("warn"));
        }
        if elapsed >= SLOW_MS
            || self.fields.get("operation") == Some(&json!("http.request"))
            || RECORDER.get().is_some_and(|r| r.debug)
        {
            emit("span.end", Value::Object(std::mem::take(&mut self.fields)));
        }
    }
}

/// Time a pure component call at its application boundary without adding a host dependency.
pub fn measure<T>(operation: &'static str, execute: impl FnOnce() -> T) -> T {
    let _span = Span::new(operation);
    execute()
}

fn register(id: u64, active: Active) {
    if let Some(recorder) = RECORDER.get() {
        if let Ok(mut spans) = recorder.active.try_lock() {
            spans.retain(|_, span| span.live.load(Ordering::Relaxed));
            if spans.len() < MAX_ACTIVE {
                spans.insert(id, active);
            } else {
                recorder.dropped.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            recorder.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}
fn unregister(id: u64) {
    if let Some(recorder) = RECORDER.get() {
        if let Ok(mut spans) = recorder.active.try_lock() {
            if let Some(active) = spans.remove(&id) {
                active.live.store(false, Ordering::Relaxed);
            }
        }
    }
}

fn valid_field(key: &str, value: &Value) -> bool {
    match key {
        "request_id" => value.as_str().is_some_and(identifier),
        "operation" | "phase" | "outcome" | "event" | "level" | "route" | "provider"
        | "error_code" => value.as_str().is_some_and(|s| {
            !s.is_empty()
                && s.len() <= 96
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
        }),
        "method" => value.as_str().is_some_and(|s| {
            matches!(
                s,
                "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS" | "OTHER"
            )
        }),
        "span_id"
        | "parent_span_id"
        | "store_span_id"
        | "elapsed_ms"
        | "duration_ms"
        | "bytes"
        | "request_bytes"
        | "response_bytes"
        | "child_pid"
        | "timeout_ms"
        | "status_code"
        | "scheduler_delay_ms"
        | "queue_wait_ms"
        | "cpu_percent"
        | "rss_bytes"
        | "peak_rss_bytes"
        | "load1"
        | "load5"
        | "load15"
        | "cpu_count"
        | "active_spans"
        | "dropped_events"
        | "sqlite_code"
        | "sqlite_extended_code"
        | "queue_depth"
        | "listen_port"
        | "count" => value.is_number(),
        _ => false,
    }
}

/// Emit only approved scalar fields. Arbitrary payloads and text are discarded.
pub fn emit(event: &'static str, fields: Value) {
    let Some(recorder) = RECORDER.get() else {
        return;
    };
    let mut fields: Map<String, Value> = fields
        .as_object()
        .into_iter()
        .flat_map(|map| map.iter())
        .filter(|(key, value)| valid_field(key, value))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    fields.insert("schema_version".into(), json!(1));
    fields.insert("event".into(), json!(event));
    fields.insert("timestamp_ms".into(), json!(wall_ms()));
    fields.insert("monotonic_ms".into(), json!(millis(recorder.start)));
    fields.insert("pid".into(), json!(std::process::id()));
    fields.insert("run_id".into(), json!(recorder.run));
    fields.entry("level").or_insert_with(|| json!("info"));
    if !fields.contains_key("request_id") {
        if let Some(request) = current_request_id() {
            fields.insert("request_id".into(), json!(request));
        }
    }
    if !fields.contains_key("span_id") {
        let active = STORE_STACK
            .with(|s| s.borrow().last().map(|(id, _)| *id))
            .or_else(|| STACK.with(|s| s.borrow().last().copied()));
        if let Some(id) = active {
            fields.insert("span_id".into(), json!(id));
        }
    }
    // Span IDs must survive JavaScript JSON readers without IEEE-754 rounding.
    for key in ["span_id", "parent_span_id"] {
        if let Some(id) = fields.get(key).and_then(Value::as_u64) {
            fields.insert(key.into(), Value::String(id.to_string()));
        }
    }
    recorder.enqueue(Value::Object(fields));
}

struct StoreLogger;
impl log::Log for StoreLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.target() == "tempo_store::diagnostics"
            && RECORDER
                .get()
                .is_some_and(|r| !r.stopped.load(Ordering::Relaxed))
    }
    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        struct Fields(Map<String, Value>);
        impl<'kvs> log::kv::VisitSource<'kvs> for Fields {
            fn visit_pair(
                &mut self,
                key: log::kv::Key<'kvs>,
                value: log::kv::Value<'kvs>,
            ) -> Result<(), log::kv::Error> {
                let value = if let Some(value) = value.to_u64() {
                    json!(value)
                } else if let Some(value) = value.to_i64() {
                    json!(value)
                } else {
                    json!(value.to_string())
                };
                if valid_field(key.as_str(), &value) {
                    self.0.insert(key.to_string(), value);
                }
                Ok(())
            }
        }
        let mut fields = Fields(Map::new());
        let _ = record.key_values().visit(&mut fields);
        let event = match record.args().as_str() {
            Some("store.start") => "store.start",
            Some("store.end") => "store.end",
            Some("sqlite.error") => "sqlite.error",
            Some("sqlite.adapter_error") => "sqlite.adapter_error",
            _ => return,
        };
        if let Some(request) = current_request_id() {
            fields.0.insert("request_id".into(), json!(request));
        }
        let id = fields
            .0
            .get("store_span_id")
            .and_then(Value::as_u64)
            .map(|v| v | (1 << 63));
        if let Some(id) = id {
            fields.0.insert("span_id".into(), json!(id));
            if event == "store.start" {
                STORE_STACK.with(|stack| {
                    let mut stack = stack.borrow_mut();
                    let parent = stack
                        .last()
                        .map(|(id, _)| *id)
                        .or_else(|| STACK.with(|s| s.borrow().last().copied()));
                    if let Some(parent) = parent {
                        fields.0.insert("parent_span_id".into(), json!(parent));
                    }
                    let live = Arc::new(AtomicBool::new(true));
                    stack.push((id, live.clone()));
                    register(
                        id,
                        Active {
                            started: Instant::now(),
                            fields: fields.0.clone(),
                            live,
                        },
                    );
                });
            } else {
                STORE_STACK.with(|stack| {
                    stack.borrow_mut().retain(|(value, live)| {
                        if *value == id {
                            live.store(false, Ordering::Relaxed);
                            false
                        } else {
                            true
                        }
                    })
                });
                unregister(id);
            }
        }
        let slow = fields
            .0
            .get("elapsed_ms")
            .and_then(Value::as_u64)
            .is_some_and(|ms| ms >= SLOW_MS);
        if slow || record.level() <= log::Level::Warn {
            fields.0.insert("level".into(), json!("warn"));
        }
        if slow || record.level() <= log::Level::Warn || RECORDER.get().is_some_and(|r| r.debug) {
            emit(event, Value::Object(fields.0));
        }
    }
    fn flush(&self) {}
}

fn monitor(stop: Arc<AtomicBool>) {
    let interval = Duration::from_secs(2);
    let mut previous = Instant::now();
    let mut previous_cpu = resource_sample().0;
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(interval);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let elapsed = previous.elapsed();
        previous = Instant::now();
        let (cpu, mut fields) = resource_sample();
        if let (Some(cpu), Some(prior)) = (cpu, previous_cpu) {
            fields.insert(
                "cpu_percent".into(),
                json!(((cpu - prior).max(0.0) / elapsed.as_secs_f64()) * 100.0),
            );
        }
        previous_cpu = cpu;
        fields.insert(
            "scheduler_delay_ms".into(),
            json!(elapsed.saturating_sub(interval).as_millis() as u64),
        );
        fields.insert("cpu_count".into(), json!(cpu_count()));
        if let Some(recorder) = RECORDER.get() {
            fields.insert(
                "dropped_events".into(),
                json!(recorder.dropped.load(Ordering::Relaxed)),
            );
            let active = recorder
                .active
                .try_lock()
                .ok()
                .map(|mut spans| {
                    spans.retain(|_, span| span.live.load(Ordering::Relaxed));
                    spans.values().cloned().collect::<Vec<_>>()
                })
                .unwrap_or_default();
            fields.insert("active_spans".into(), json!(active.len()));
            for span in active
                .into_iter()
                .filter(|span| millis(span.started) >= 1000)
                .take(12)
            {
                let mut fields = span.fields;
                fields.insert("elapsed_ms".into(), json!(millis(span.started)));
                fields.insert("level".into(), json!("warn"));
                emit("operation.stalled", Value::Object(fields));
            }
        }
        emit("runtime.sample", Value::Object(fields));
    }
}

fn resource_sample() -> (Option<f64>, Map<String, Value>) {
    let mut fields = Map::new();
    #[cfg(unix)]
    {
        // getrusage/getloadavg write only into these correctly sized local values.
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        let cpu = if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } == 0 {
            let usage = unsafe { usage.assume_init() };
            let seconds = |v: libc::timeval| v.tv_sec as f64 + v.tv_usec as f64 / 1_000_000.0;
            let peak = usage.ru_maxrss.max(0) as u64;
            #[cfg(target_os = "macos")]
            fields.insert("peak_rss_bytes".into(), json!(peak));
            #[cfg(not(target_os = "macos"))]
            fields.insert("peak_rss_bytes".into(), json!(peak.saturating_mul(1024)));
            Some(seconds(usage.ru_utime) + seconds(usage.ru_stime))
        } else {
            None
        };
        let mut loads = [0f64; 3];
        if unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) } == 3 {
            for (key, value) in ["load1", "load5", "load15"].into_iter().zip(loads) {
                fields.insert(key.into(), json!(value));
            }
        }
        #[cfg(target_os = "linux")]
        if let Ok(statm) = fs::read_to_string("/proc/self/statm") {
            if let Some(resident) = statm
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
            {
                let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
                if page > 0 {
                    fields.insert(
                        "rss_bytes".into(),
                        json!(resident.saturating_mul(page as u64)),
                    );
                }
            }
        }
        (cpu, fields)
    }
    #[cfg(not(unix))]
    {
        (None, fields)
    }
}

struct RotatingWriter {
    base: PathBuf,
    file: File,
    bytes: u64,
    limit: u64,
}
impl RotatingWriter {
    fn new(directory: &Path, run: &str, limit: u64) -> io::Result<Self> {
        if fs::symlink_metadata(directory).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(io::Error::other("diagnostic directory is a symlink"));
        }
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(directory)?;
        prune_runs(directory);
        let base = directory.join(format!("tempo-{run}.jsonl"));
        let file = private_file(&base)?;
        Ok(Self {
            base,
            file,
            bytes: 0,
            limit,
        })
    }
    fn record(&mut self, value: &Value) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        if bytes.len() > 8192 {
            return Ok(());
        }
        if self.bytes > 0 && self.bytes.saturating_add(bytes.len() as u64) > self.limit {
            let one = PathBuf::from(format!("{}.1", self.base.display()));
            let two = PathBuf::from(format!("{}.2", self.base.display()));
            if two.exists() {
                fs::remove_file(&two)?;
            }
            if one.exists() {
                fs::rename(&one, &two)?;
            }
            fs::rename(&self.base, &one)?;
            self.file = private_file(&self.base)?;
            self.bytes = 0;
        }
        self.file.write_all(&bytes)?;
        self.bytes += bytes.len() as u64;
        Ok(())
    }
}
fn private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
fn prune_runs(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    // Group rotations too: a stopped writer can have .1/.2 without a base file.
    let mut grouped = BTreeMap::<PathBuf, SystemTime>::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let base = name
            .strip_suffix(".1")
            .or_else(|| name.strip_suffix(".2"))
            .unwrap_or(&name);
        let Some(run) = base
            .strip_prefix("tempo-")
            .and_then(|v| v.strip_suffix(".jsonl"))
        else {
            continue;
        };
        let parts = run.split('-').collect::<Vec<_>>();
        if parts.len() != 3
            || !parts[0].bytes().all(|b| b.is_ascii_hexdigit())
            || !parts[2].bytes().all(|b| b.is_ascii_hexdigit())
        {
            continue;
        }
        let Ok(pid) = parts[1].parse::<u32>() else {
            continue;
        };
        if pid == 0 || pid > i32::MAX as u32 || pid == std::process::id() {
            continue;
        }
        #[cfg(unix)]
        {
            let status = unsafe { libc::kill(pid as libc::pid_t, 0) };
            if status == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                continue;
            }
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        grouped
            .entry(directory.join(base))
            .and_modify(|time| *time = (*time).max(modified))
            .or_insert(modified);
    }
    let mut runs = grouped
        .into_iter()
        .map(|(path, time)| (time, path))
        .collect::<Vec<_>>();
    runs.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, base) in runs.into_iter().skip(4) {
        for suffix in ["", ".1", ".2"] {
            let _ = fs::remove_file(format!("{}{suffix}", base.display()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn context_is_delivery_only_and_restores_after_panic() {
        let context = RequestContext::new(Some("01234567-89ab-cdef"));
        assert_eq!(current_request_id().as_deref(), Some(context.id()));
        let _ =
            std::panic::catch_unwind(|| with_context(Some("abcdef01".into()), || panic!("probe")));
        assert_eq!(current_request_id().as_deref(), Some(context.id()));
        let invalid = RequestContext::new(Some("secret/path?token=private"));
        assert!(identifier(invalid.id()));
        assert_ne!(invalid.id(), "secret/path?token=private");
    }
    #[test]
    fn scalar_allowlist_excludes_content_paths_and_messages() {
        assert!(!valid_field("pgn", &json!("1. e4 e5")));
        assert!(!valid_field(
            "error_code",
            &json!("SQL /private/path password")
        ));
        assert!(!valid_field("route", &json!("/api/study?token=private")));
        assert!(!valid_field("operation", &json!({"secret":true})));
        assert!(valid_field("elapsed_ms", &json!(5)));
        assert!(valid_field("operation", &json!("sqlite.begin_immediate")));
    }
    #[test]
    fn rotation_bounds_files_and_keeps_complete_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = RotatingWriter::new(dir.path(), "abcdef-999999-abcdef", 100).unwrap();
        for n in 0..30 {
            writer.record(&json!({"event":"test","n":n})).unwrap();
        }
        drop(writer);
        let files = fs::read_dir(dir.path()).unwrap().collect::<Vec<_>>();
        assert_eq!(files.len(), 3);
        for file in files {
            let path = file.unwrap().path();
            assert!(fs::metadata(&path).unwrap().len() <= 100);
            for line in fs::read_to_string(path).unwrap().lines() {
                serde_json::from_str::<Value>(line).unwrap();
            }
        }
    }
    #[test]
    fn full_channel_does_not_wait_for_writer() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let recorder = Recorder {
            sender,
            run: "test".into(),
            start: Instant::now(),
            debug: true,
            dropped: AtomicU64::new(0),
            active: Mutex::new(BTreeMap::new()),
            stopped: Arc::new(AtomicBool::new(false)),
        };
        recorder.enqueue(json!({"event":"first"}));
        let started = Instant::now();
        recorder.enqueue(json!({"event":"second"}));
        assert_eq!(recorder.dropped.load(Ordering::Relaxed), 1);
        assert!(started.elapsed() < Duration::from_millis(100));
    }
    #[test]
    fn retention_prunes_orphan_rotations_but_preserves_live_runs() {
        let dir = tempfile::tempdir().unwrap();
        for run in 0..8 {
            fs::write(
                dir.path()
                    .join(format!("tempo-{run:x}-2147483647-a.jsonl.1")),
                b"{}\n",
            )
            .unwrap();
        }
        let live = dir
            .path()
            .join(format!("tempo-a-{}-a.jsonl.1", std::process::id()));
        fs::write(&live, b"{}\n").unwrap();
        prune_runs(dir.path());
        assert!(live.exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 5);
    }
    #[test]
    fn resource_sample_needs_no_subprocess_or_database() {
        let (cpu, fields) = resource_sample();
        #[cfg(unix)]
        {
            assert!(cpu.is_some());
            assert!(fields.contains_key("peak_rss_bytes"));
        }
    }
}
