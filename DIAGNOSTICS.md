# Diagnosing Tempo waits

Tempo records structured timing evidence so a browser timeout can be followed
through the server, application work, database operations, and background work.
The optional host collector records competing processes and memory/load at the
same time. Logging belongs to these existing owners; it does not introduce a
new service, repository, database, or scheduling policy.

## Capture a wait

1. Reproduce the wait while the collector is running. From the assembled
   workspace, run the command below. The example log directory matches a
   database at `.local/usable-release/study.sqlite`; use the `diagnostics`
   directory beside the actual registered database when it differs.
2. After the warning, use the diagnostics download button in the Study error
   display or Games header to export the browser's bounded request history.
   The developer-console equivalent is `window.tempoDiagnostics.download()`.
   This is a local download; it makes no Study request. Keep the browser open
   until the export because the ring is intentionally not persisted.
3. Keep the host JSON and browser JSON together. Match `request_id` first,
   followed by `run_id` and server `pid`. A browser attempt receives a new
   diagnostic request ID on each delivery; its durable command identity and
   exact retry body stay unchanged.

`runtime.listening` records the listener's PID and `listen_port`, so installed
and development copies can be distinguished without recording executable paths.

```sh
python3 tempo/tools/diagnose.py \
  --duration 60 \
  --log-dir .local/usable-release/diagnostics \
  --output tempo-diagnosis.json
```

`--pid 12345` additionally retains that exact Tempo process in every sample,
even if it is outside the top 20 CPU users. Read the actual PID from the
runtime log or existing managed-service status. Without it, the collector
still records the top 20 processes. It does not discover or launch a Tempo
instance, read SQLite, contact the app, rebuild anything, or restart a service.

The collector defaults to 30 seconds with a 2-second interval. Duration is
bounded to 1–300 seconds and interval to 1–30 seconds. Ctrl+C saves completed
samples. Each `ps` or macOS `vm_stat` query has a 2-second deadline and a bounded
output budget. A timed-out diagnostic child is terminated; observed processes
are never signaled. Unavailable samples appear as explicit error codes.

`--output` accepts a new JSON filename or an existing directory. The default
is a timestamped filename in the current directory. Existing files are never
overwritten. New output is created with owner-only permissions. Python 3.9+
and the operating system's standard commands are sufficient.

## Runtime recording

The host defaults to `TEMPO_LOG=info`. It records request boundaries, errors,
operations taking at least 250 ms, watchdog observations, and resource samples.
`TEMPO_LOG=debug` also records the detailed operation starts and finishes.
`TEMPO_LOG=off` disables runtime recording. Set `TEMPO_LOG_DIR` to select a
different destination; otherwise logs live in `diagnostics` beside the database.
Apply runtime settings through the existing launch environment when preparing
the corresponding build. Shell variables affect a process only when its launcher
passes them through; setting a shell variable does not reconfigure an already
running LaunchAgent.

Files are named `tempo-<run_id>.jsonl`, with `.1` and `.2` rotations. Each file
is capped at 5 MiB. At startup, retention keeps four inactive historical run
sets, the new run, and any runs whose process is alive or cannot safely be
classified as stopped. A single active instance therefore normally retains at
most five run sets (75 MiB); concurrent active processes and ambiguous PIDs can
raise that total. Each individual run is still limited to three files (15 MiB).
A bounded asynchronous writer keeps file I/O outside request work. The
`dropped_events` counter includes recorder-delivery failures and missed
best-effort active-span registrations; it is not exclusively a count of full
channel drops. A missing event therefore does not establish that an operation
never ran. Detailed logging can still add CPU and allocation cost,
so use the default level for routine operation and debug for a reproduction.

The event schema contains timestamps, monotonic timings, generated correlation
IDs, static operation names, outcomes, status/error codes, sizes, process IDs,
and resource counters. It excludes request/response bodies, PGN, FEN, move
answers, credentials, query strings, raw SQL, and free-form exception text.
Existing application stdout protocols and the startup banner retain their
meaning. Diagnostic success or failure does not establish application-command
acceptance.

The browser retains at most 500 events in memory. Its transport observations
distinguish waiting for response headers, reading/decoding the body, and the
deadline ending. Browser timer delay is recorded separately from the configured
wait budget. Optional `X-Tempo-Request-Id`, `X-Tempo-Run-Id`, and `Server-Timing`
response headers aid correlation; they do not become durable wire fields.

## Read the evidence

| Observation | Supported conclusion and useful next check |
| --- | --- |
| Browser begins a request; matching server request start arrives much later | Delay occurred before application handling. Check concurrent background spans, process load, and phone/proxy connectivity. The evidence alone cannot separate network delay from server queueing. |
| A watchdog repeatedly names the same running operation | That operation had not finished when sampled. Look at its nested application or Store spans to locate the wait. |
| A database operation takes about 5 seconds and reports SQLite busy/locked | A database lock wait is recorded. The lock holder still needs to be identified; elapsed time alone is insufficient. |
| Tempo consumes high CPU and one application or engine stage is long | Work in that process is a plausible contributor. Correlate the same interval and request before attributing the timeout to it. |
| Compiler/engine processes dominate CPU, host load rises, and sampler/watchdog ticks arrive late | Resource contention is plausible. Compare a reproduction when those jobs are idle; this does not prove causation by itself. |
| Tempo uses little CPU while an operation stays active and host resources look normal | Waiting or blocking is plausible. Check database, subprocess, provider, and body-read phases. A sampled snapshot cannot rule out short bursts between samples. |
| Server records a completed response but the browser times out reading it | Inspect client delivery/body processing, browser scheduling, and the network path. The ordinary command receipt/recovery contract remains the authority on acceptance. |
| Browser deadline fires well after its configured budget | The browser's timer was delayed. Busy JavaScript, a suspended computer, or background-tab timer throttling can contribute; it is not a direct server-duration measurement. |

Foreground Study and accepted writes use the serialized application loop. The
personal tactics report has one worker and one waiting request. Report admission
can return busy; accepted work keeps its delivery ID across the worker and
response. Debug logs distinguish time waiting for the worker, cold report
construction, completed page reads and addressed position detail. Study
presence records delivery without retaining tab identities or lease payloads.
Tactical acceptance spans identify the addressed Store phases and commit.
SQLite's busy timeout and all existing client deadlines stay unchanged.

Use monotonic elapsed durations within each process. Wall-clock timestamps
help align host snapshots, but a phone and Mac need not have synchronized clocks;
correlation IDs are a stronger match. Treat `span_id` and `parent_span_id` as
opaque decimal strings; converting them to JavaScript numbers can lose precision.
Process CPU percentages from `ps` use
platform-specific averaging, not necessarily the collector's sample interval.
`rss_bytes` is current resident memory; `peak_rss_bytes` is the process's
high-water mark and cannot establish current memory pressure. Runtime sampling
reports the peak on macOS; the host collector supplies current RSS through
`ps`. Memory compression and swap counters are evidence to compare over time, not a
standalone declaration of memory pressure. Executable-name labels such as
`compiler_or_build_tool` and `chess_engine` are hints; generic Python or Node
processes cannot be attributed to a particular job without additional evidence.

## Export bounds and privacy

The host export includes known process labels and broad role hints, PIDs and
parent PIDs, sampled CPU/RSS, host CPU count/load, and selected memory counters.
Unknown executables appear as `other`. The export omits private executable names and
paths, command lines, process environments, hostnames and usernames.

With `--log-dir`, the collector reads at most five matching regular log files,
at most the final 512 KiB from each, and retains at most 2,000 allowlisted events.
Symlinked log files, unrelated filenames, malformed records, unknown schemas,
raw text, and unknown/nested event fields are excluded. It records truncation
and skip counts. Therefore the exported bundle is a bounded diagnostic sample,
not a complete historical audit. Original log files stay in place.

## Verification

Run the collector's focused checks with
`python3 -m unittest discover -s tempo/tools/tests -p test_diagnose.py -v`
from the assembled workspace. They cover process-name and path redaction, units on both
platforms, actual child timeout and output bounds, log filtering and truncation,
unavailable tools, interruption, invalid duration, and overwrite prevention.
macOS parsing fixtures are separate from a real macOS execution; record which
platform was actually used when reporting a capture.
