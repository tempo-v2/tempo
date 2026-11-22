"""Exercise diagnostic observation against the production HTTP/SQLite binary.

Build with ``cargo build -p tempo --bin tempo`` from the assembled workspace,
then run ``python3 tempo/tests/runtime_diagnostics.py -v``. TEMPO_BIN can select
an already-built binary. All databases, UI artifacts, and logs are disposable.
No engine, public game provider, browser, or installed Tempo instance is used.
"""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
import errno
import http.client
import json
import os
from pathlib import Path
import queue
import re
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from urllib.parse import urlsplit
import uuid


TEMPO_SOURCE = Path(__file__).resolve().parents[1]
WORKSPACE = TEMPO_SOURCE.parent
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", WORKSPACE / "target"))
BINARY = Path(os.environ.get("TEMPO_BIN", TARGET / "debug" / "tempo")).resolve()
STATUS_BODY = b'{"command":"study_status"}'
MAX_LOG_BYTES = 2 * 1024 * 1024


def log_events(directory: Path) -> list[dict]:
    """Read only bounded complete lines; a concurrent final line may be partial."""
    events = []
    remaining = MAX_LOG_BYTES
    for path in sorted(directory.glob("tempo-*.jsonl"))[-3:]:
        with path.open("rb") as source:
            content = source.read(remaining)
        remaining -= len(content)
        for line in content.splitlines(keepends=True):
            if not line.endswith(b"\n"):
                continue
            value = json.loads(line)
            if isinstance(value, dict):
                events.append(value)
        if remaining == 0:
            break
    return events


def wait_event(directory: Path, predicate, timeout: float = 3.0) -> dict:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for event in reversed(log_events(directory)):
            if predicate(event):
                return event
        time.sleep(0.02)
    raise AssertionError("Expected diagnostic event did not arrive before its bounded wait")


class Server:
    """Bounded production-process lifetime; never uses the managed launcher."""

    def __init__(self, database: Path, ui: Path, logs: Path, mode: str = "debug"):
        env = os.environ.copy()
        env.update(TEMPO_LOG=mode, TEMPO_LOG_DIR=str(logs))
        # An inherited private-phone registration is unrelated to this disposable host.
        env.pop("TEMPO_PHONE_ORIGIN", None)
        env.pop("TEMPO_PHONE_TAILSCALE_LOGIN", None)
        self.logs = logs
        self.stderr = tempfile.TemporaryFile(mode="w+b")
        self.process = subprocess.Popen(
            [str(BINARY), "serve", str(database), str(ui), "0"],
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=self.stderr,
            text=True,
        )
        lines: queue.Queue[str | None] = queue.Queue(maxsize=32)

        def read_banner():
            try:
                for line in self.process.stdout:
                    try:
                        lines.put_nowait(line.rstrip("\n"))
                    except queue.Full:
                        break
            finally:
                try:
                    lines.put_nowait(None)
                except queue.Full:
                    pass

        self.reader = threading.Thread(target=read_banner, daemon=True)
        self.reader.start()
        try:
            first = lines.get(timeout=6)
            match = re.fullmatch(r"Tempo study at (http://127\.0\.0\.1:\d+)", first or "")
            if not match:
                raise AssertionError(
                    f"Startup banner was missing or displaced: {first!r}; {self.error_text()}"
                )
            self.url = match[1]
            parsed = urlsplit(self.url)
            self.host = parsed.hostname
            self.port = parsed.port
        except BaseException:
            self.stop()
            raise

    def error_text(self) -> str:
        self.stderr.seek(0)
        return self.stderr.read(8192).decode("utf-8", errors="replace")

    def connection(self, timeout: float = 8) -> http.client.HTTPConnection:
        return http.client.HTTPConnection(self.host, self.port, timeout=timeout)

    def request(self, body=STATUS_BODY, *, path="/api/study", headers=None, method="POST"):
        request_id = str(uuid.uuid4())
        fields = {"Content-Type": "application/json", "X-Tempo-Request-Id": request_id}
        fields.update(headers or {})
        connection = self.connection()
        try:
            connection.request(method, path, body=body, headers=fields)
            response = connection.getresponse()
            payload = response.read()
            return response.status, response.headers, payload, fields["X-Tempo-Request-Id"]
        finally:
            connection.close()

    def stop(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=2)
        if self.process.stdout:
            self.process.stdout.close()
        self.reader.join(timeout=0.2)
        self.stderr.close()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.stop()


class RuntimeDiagnostics(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not BINARY.is_file():
            raise RuntimeError(f"Build the production Tempo binary first, or set TEMPO_BIN: {BINARY}")
        # Skip only an explicit platform denial, never a generic startup/test error.
        try:
            with socket.socket() as probe:
                probe.bind(("127.0.0.1", 0))
        except OSError as error:
            if error.errno in (errno.EPERM, errno.EACCES):
                raise unittest.SkipTest(f"This executor explicitly denies local TCP: {error}") from error
            raise

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="tempo-diagnostics-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.database = self.root / "study.sqlite"
        self.ui = self.root / "ui"
        (self.ui / "assets").mkdir(parents=True)
        (self.ui / "index.html").write_text("<!doctype html><title>Diagnostic fixture</title>")
        (self.ui / "assets" / "fixture.js").write_text("export const fixture = true;\n")
        subprocess.run(
            [str(BINARY), "init", str(self.database)],
            check=True,
            capture_output=True,
            timeout=5,
        )

    def assert_correlated(self, directory: Path, request_id: str):
        terminal = wait_event(
            directory,
            lambda e: e.get("request_id") == request_id
            and e.get("event") == "span.end"
            and e.get("operation") == "http.request",
        )
        events = [e for e in log_events(directory) if e.get("request_id") == request_id]
        self.assertTrue(any(e.get("operation", "").startswith("app.") for e in events))
        self.assertTrue(any(e.get("event") == "store.end" for e in events))
        self.assertTrue(any(e.get("operation") == "http.read_body" for e in events))
        self.assertTrue(any(e.get("operation") == "http.decode_body" for e in events))
        self.assertTrue(any(e.get("event") == "http.response" and e.get("status_code") == 200 for e in events))
        self.assertTrue(all(e["run_id"] == terminal["run_id"] for e in events))
        return terminal

    def test_http_store_correlation_and_content_redaction(self):
        logs = self.root / "logs"
        with Server(self.database, self.ui, logs) as server:
            status, headers, payload, request_id = server.request()
            self.assertEqual(status, 200)
            self.assertEqual(json.loads(payload)["status"], "empty")
            self.assertEqual(headers.get("X-Tempo-Request-Id"), request_id)
            self.assertRegex(headers.get("X-Tempo-Run-Id", ""), r"^[a-fA-F0-9-]{8,64}$")
            terminal = self.assert_correlated(logs, request_id)
            self.assertEqual(headers["X-Tempo-Run-Id"], terminal["run_id"])

            secrets = (
                "diagnostic-header-secret-never-log",
                "diagnostic-query-secret-never-log",
                '[Event "private-pgn-do-not-log"] 1. e4 e5 *',
                "7k/6Q1/6K1/8/8/8/8/8 b - - 0 50",
                "private-invalid-request-id",
            )
            invalid_body = json.dumps({"command": "invalid", "pgn": secrets[2], "fen": secrets[3]}).encode()
            _, invalid_headers, invalid_payload, _ = server.request(
                invalid_body,
                headers={"Authorization": f"Bearer {secrets[0]}", "X-Tempo-Request-Id": secrets[4]},
            )
            self.assertEqual(json.loads(invalid_payload)["code"], "invalid_command")
            regenerated = invalid_headers["X-Tempo-Request-Id"]
            self.assertNotEqual(regenerated, secrets[4])
            wait_event(
                logs,
                lambda e: e.get("request_id") == regenerated
                and e.get("event") == "application.error"
                and e.get("error_code") == "invalid_command",
            )
            self.assertRegex(regenerated, r"^[a-fA-F0-9-]{8,64}$")
            _, _, asset, asset_request = server.request(
                None, method="GET", path=f"/assets/fixture.js?token={secrets[1]}"
            )
            self.assertIn(b"export const fixture", asset)
            wait_event(logs, lambda e: e.get("request_id") == asset_request and e.get("event") == "http.response")
            events = log_events(logs)
            # Inspect decoded scalar values, so JSON escaping cannot hide a leak.
            rendered = "\n".join(str(value) for event in events for value in event.values())
            for secret in secrets:
                self.assertNotIn(secret, rendered)
            self.assertNotIn(str(self.database), rendered)

    def test_disabled_and_failed_writer_preserve_the_http_reply(self):
        expected = None
        for mode in ("debug", "info", "off", "failed_directory"):
            with self.subTest(mode=mode):
                logs = self.root / mode
                if mode == "failed_directory":
                    logs.write_text("A file cannot become the diagnostic directory.")
                with Server(self.database, self.ui, logs, mode=mode if mode in ("off", "info") else "debug") as server:
                    status, headers, payload, request_id = server.request()
                    self.assertEqual(status, 200)
                    self.assertEqual(headers.get("X-Tempo-Request-Id"), request_id)
                    result = json.loads(payload)
                    if expected is None:
                        expected = result
                    else:
                        self.assertEqual(result, expected)
                    if mode == "info":
                        self.assertIn("X-Tempo-Run-Id", headers)
                        wait_event(
                            logs,
                            lambda e: e.get("request_id") == request_id
                            and e.get("event") == "span.end"
                            and e.get("operation") == "http.request",
                        )
                        _, _, invalid, invalid_id = server.request(b'{"command":"invalid"}')
                        self.assertEqual(json.loads(invalid)["code"], "invalid_command")
                        wait_event(
                            logs,
                            lambda e: e.get("request_id") == invalid_id
                            and e.get("event") == "application.error"
                            and e.get("error_code") == "invalid_command",
                        )
                    elif mode == "off":
                        self.assertNotIn("X-Tempo-Run-Id", headers)
                        self.assertFalse(logs.exists())
                    elif mode == "failed_directory":
                        self.assertTrue(logs.is_file())
                        # A second independent request also remains responsive.
                        self.assertEqual(json.loads(server.request()[2]), expected)

    def test_report_retirement_preserves_games_context_and_study(self):
        logs = self.root / "tactics-logs"
        retired_ids = []
        with Server(self.database, self.ui, logs) as server:
            status, _, payload, study_id = server.request()
            self.assertEqual(status, 200)
            dataset = json.loads(payload)["dataset_id"]
            for path in ("/api/games/tactics/report", "/api/games/tactics/position"):
                status, headers, payload, delivery = server.request(
                    json.dumps({"dataset_id": dataset, "id": 1}).encode(), path=path,
                )
                self.assertEqual(status, 410)
                self.assertEqual(json.loads(payload)["code"], "retired_feature")
                self.assertEqual(headers["X-Tempo-Request-Id"], delivery)
                retired_ids.append(delivery)

            status, _, payload, _ = server.request(None, path="/api/context", method="GET")
            self.assertEqual(status, 200)
            self.assertEqual(json.loads(payload)["dataset_id"], dataset)
            status, _, payload, _ = server.request(None, path="/api/games", method="GET")
            self.assertEqual(status, 200)
            self.assertEqual(json.loads(payload)["results"], [])
            self.assert_correlated(logs, study_id)
            for delivery in retired_ids:
                wait_event(logs, lambda e: e.get("request_id") == delivery and e.get("event") == "http.response")
                events = [e for e in log_events(logs) if e.get("request_id") == delivery]
                self.assertFalse(any(
                    e.get("operation", "").startswith(("tactics.report.", "sqlite.", "app."))
                    for e in events
                ))

        output = self.root / "offline-export.json"
        subprocess.run(
            [sys.executable, str(TEMPO_SOURCE / "tools" / "diagnose.py"),
             "--duration", "1", "--interval", "1", "--log-dir", str(logs), "--output", str(output)],
            check=True,
            capture_output=True,
            timeout=5,
        )
        bundle = json.loads(output.read_text())
        self.assertEqual(bundle["diagnostic_logs"]["status"], "captured")
        self.assertTrue(bundle["samples"])
        events = bundle["diagnostic_logs"]["events"]
        self.assertLessEqual(len(events), bundle["diagnostic_logs"]["bounds"]["events"])
        for delivery in retired_ids:
            exported = next(
                event for event in events if event.get("request_id") == delivery and event["event"] == "http.response"
            )
            self.assertEqual(exported["status_code"], 410)
        self.assertNotIn(dataset, json.dumps(events))

    def test_position_evidence_retirement_leaves_replay_and_authoring_available(self):
        logs = self.root / "position-retirement-logs"

        def post(server, path, payload):
            return server.request(json.dumps(payload).encode(), path=path)

        with Server(self.database, self.ui, logs) as server:
            status, _, context, _ = server.request(None, path="/api/context", method="GET")
            self.assertEqual(status, 200)
            dataset = json.loads(context)["dataset_id"]
            for path in ("/api/analysis/evidence", "/api/analysis/result"):
                status, headers, payload, delivery = post(server, path, {"request_id": "old-reader"})
                self.assertEqual(status, 410)
                self.assertEqual(json.loads(payload)["code"], "retired_feature")
                self.assertEqual(headers["X-Tempo-Request-Id"], delivery)
                wait_event(logs, lambda e: e.get("request_id") == delivery and e.get("event") == "http.response")
                events = [e for e in log_events(logs) if e.get("request_id") == delivery]
                self.assertFalse(any(
                    e.get("operation", "").startswith(("evidence.", "sqlite.", "app."))
                    for e in events
                ))

            status, _, imported, _ = post(server, "/api/games/import", {
                "dataset_id": dataset, "command_id": "fixture-game", "learner": "Learner",
                "pgn": '[White "Learner"]\n[Black "Opponent"]\n[Result "1-0"]\n\n1. e4 e5 1-0',
            })
            self.assertEqual(status, 200)
            self.assertEqual(json.loads(imported)["new_games"], 1)
            status, _, library, _ = post(server, "/api/games/library", {"dataset_id": dataset})
            self.assertEqual(status, 200)
            game, = json.loads(library)["entries"]
            status, _, replay, _ = post(server, "/api/games/review", {
                "dataset_id": dataset, "source": game["source"], "game_id": game["game_id"],
                "learner": game["learner"], "repertoire_id": None,
            })
            self.assertEqual(status, 200)
            replay = json.loads(replay)
            self.assertEqual(replay["status"], "review")
            self.assertEqual(len(replay["positions"]), 3)
            self.assertIsNone(replay["comparison"])

            draft = {"dataset_id": dataset, "name": "Opening", "learner_color": "white", "pgn": "1. d4 d5 2. Bf4 *"}
            status, _, preview, _ = post(server, "/api/repertoires", {"command": "preview", **draft})
            self.assertEqual(status, 200)
            self.assertEqual(json.loads(preview)["status"], "preview")
            status, _, saved, _ = post(server, "/api/repertoires", {
                "command": "save", **draft, "token": json.loads(preview)["token"],
            })
            self.assertEqual(status, 200)
            self.assertEqual(json.loads(saved)["status"], "repertoires")

    def test_tactical_jobs_and_presence_retirement_do_no_work(self):
        logs = self.root / "tactical-jobs-retirement-logs"
        with Server(self.database, self.ui, logs) as server:
            _, _, context, _ = server.request(None, path="/api/context", method="GET")
            dataset = json.loads(context)["dataset_id"]
            for path, command in (
                ("/api/study/presence", {"tab": "7a791296-6215-4d60-ae7c-58bab00ce975", "sequence": 1, "state": "active"}),
                ("/api/games/tactics", {"dataset_id": dataset}),
                ("/api/games/tactics/command", {"action": "resume", "dataset_id": dataset}),
                ("/api/games/tactics/command", {"action": "retry", "dataset_id": dataset}),
            ):
                status, _, payload, delivery = server.request(json.dumps(command).encode(), path=path)
                self.assertEqual(status, 410)
                self.assertEqual(json.loads(payload)["code"], "retired_feature")
                wait_event(logs, lambda e: e.get("request_id") == delivery and e.get("event") == "http.response")
                events = [e for e in log_events(logs) if e.get("request_id") == delivery]
                self.assertFalse(any(
                    e.get("operation", "").startswith(("tactics.", "detection.", "study.presence", "sqlite.", "app."))
                    for e in events
                ))

            status, _, study, _ = server.request(json.dumps({"command": "next"}).encode())
            self.assertEqual(status, 200)
            self.assertEqual(json.loads(study)["dataset_id"], dataset)
            self.assertFalse(any(
                e.get("operation") in ("loop.detection_apply", "loop.tactics_apply")
                for e in log_events(logs)
            ))

    def test_manual_detector_retirement_does_no_work_and_preserves_capture(self):
        logs = self.root / "manual-detector-retirement-logs"
        with Server(self.database, self.ui, logs) as server:
            for method, path in (
                ("GET", "/api/games/detectors"),
                ("POST", "/api/games/detection"),
                ("POST", "/api/games/detection-result"),
            ):
                status, _, payload, delivery = server.request(
                    None if method == "GET" else b"{}", method=method, path=path
                )
                self.assertEqual(status, 410)
                self.assertEqual(json.loads(payload)["code"], "retired_feature")
                wait_event(logs, lambda e: e.get("request_id") == delivery and e.get("event") == "http.response")
                events = [e for e in log_events(logs) if e.get("request_id") == delivery]
                self.assertFalse(any(
                    e.get("operation", "").startswith(("detection.", "sqlite.", "app."))
                    for e in events
                ))

            status, _, context, _ = server.request(None, path="/api/context", method="GET")
            self.assertEqual(status, 200)
            capture = {
                "dataset_id": json.loads(context)["dataset_id"],
                "pgn": '[SetUp "1"]\n[FEN "4k3/8/8/8/8/8/4P3/4K3 w - - 0 1"]\n1.e4 *',
            }
            status, _, preview, _ = server.request(
                json.dumps({"command": "preview", **capture}).encode(), path="/api/tactics"
            )
            self.assertEqual(status, 200)
            preview = json.loads(preview)
            self.assertEqual(preview["rows"][0]["status"], "valid")
            save = json.dumps({
                "command": "save", **capture, "token": preview["token"], "selected": [0],
            }).encode()
            status, _, reply, _ = server.request(save, path="/api/tactics")
            self.assertEqual(status, 200)
            self.assertEqual(json.loads(reply)["status"], "tactics_saved")
            status, _, replayed, _ = server.request(save, path="/api/tactics")
            self.assertEqual(status, 200)
            self.assertEqual(replayed, reply)

    def test_coverage_scan_and_explorer_retirement_preserves_starting_line_save(self):
        logs = self.root / "coverage-scan-retirement-logs"
        with Server(self.database, self.ui, logs) as server:
            for method, path in (
                ("POST", "/api/games/coverage"),
                ("POST", "/api/games/coverage/status"),
                ("GET", "/api/games/gap-explorer"),
                ("POST", "/api/games/gap-explorer"),
            ):
                status, _, payload, delivery = server.request(
                    None if method == "GET" else b"{}", method=method, path=path
                )
                self.assertEqual(status, 410)
                self.assertEqual(json.loads(payload)["code"], "retired_feature")
                wait_event(logs, lambda e: e.get("request_id") == delivery and e.get("event") == "http.response")
                events = [e for e in log_events(logs) if e.get("request_id") == delivery]
                self.assertFalse(any(
                    e.get("operation", "").startswith(("coverage.", "explorer.", "sqlite.", "app."))
                    for e in events
                ))

            def post(path, command):
                status, _, payload, _ = server.request(json.dumps(command).encode(), path=path)
                self.assertEqual(status, 200)
                result = json.loads(payload)
                self.assertNotEqual(result.get("status"), "error", result.get("code"))
                return result

            _, _, context, _ = server.request(None, path="/api/context", method="GET")
            dataset = json.loads(context)["dataset_id"]
            draft = {"dataset_id": dataset, "name": "Coverage save fixture", "learner_color": "white",
                     "pgn": "1. e4 e5 2. Nf3 *"}
            preview = post("/api/repertoires", {"command": "preview", **draft})
            saved_book = post("/api/repertoires", {"command": "save", **draft, "token": preview["token"]})
            repertoire = next(item["id"] for item in saved_book["repertoires"] if item["name"] == draft["name"])
            old_command = {"dataset_id": dataset, "repertoire_id": repertoire,
                           "candidate": ["e2e4"], "save_at_revision": 0}
            old_route = "/api/games/coverage/starting-line"
            saved = post(old_route, old_command)["saved"]
            self.assertEqual(saved, {"revision": 1, "moves": ["e2e4"]})
            self.assertEqual(post(old_route + "/save", old_command)["saved"], saved)
            self.assertEqual(post(old_route, {
                "dataset_id": dataset, "repertoire_id": repertoire, "candidate": None,
            })["saved"], saved)

    def test_gap_line_preview_retirement_does_no_work(self):
        logs = self.root / "gap-line-retirement-logs"
        with Server(self.database, self.ui, logs) as server:
            for path in ("/api/games/gap-line", "/api/games/gap-line/status"):
                status, _, payload, delivery = server.request(b"{}", path=path)
                self.assertEqual(status, 410)
                self.assertEqual(json.loads(payload)["code"], "retired_feature")
                wait_event(logs, lambda e: e.get("request_id") == delivery and e.get("event") == "http.response")
                events = [e for e in log_events(logs) if e.get("request_id") == delivery]
                self.assertFalse(any(
                    e.get("operation", "").startswith(("gap_line.", "engine.", "sqlite.", "app."))
                    for e in events
                ))

    def test_standalone_gap_analysis_retirement_does_no_work(self):
        logs = self.root / "gap-analysis-retirement-logs"
        with Server(self.database, self.ui, logs) as server:
            for method, path in (
                ("GET", "/api/games/gap-analysis"),
                ("POST", "/api/games/gap-analysis"),
                ("POST", "/api/games/gap-analysis/status"),
            ):
                status, _, payload, delivery = server.request(
                    None if method == "GET" else b"{}", method=method, path=path
                )
                self.assertEqual(status, 410)
                self.assertEqual(json.loads(payload)["code"], "retired_feature")
                wait_event(logs, lambda e: e.get("request_id") == delivery and e.get("event") == "http.response")
                events = [e for e in log_events(logs) if e.get("request_id") == delivery]
                self.assertFalse(any(
                    e.get("operation", "").startswith(("gap_analysis.", "sqlite.", "app."))
                    for e in events
                ))

    def test_real_sqlite_wait_has_request_stall_and_recovery_evidence(self):
        logs = self.root / "lock-logs"
        with Server(self.database, self.ui, logs) as server:
            baseline = json.loads(server.request()[2])
            request_id = str(uuid.uuid4())
            # Wait for the ingress to receive this request while withholding the
            # rest of its legal body so the application has not claimed it.
            body = STATUS_BODY + b" " * 2048
            connection = server.connection(timeout=8)
            lock = sqlite3.connect(self.database, timeout=2)
            try:
                connection.putrequest("POST", "/api/study")
                connection.putheader("Content-Type", "application/json")
                connection.putheader("Content-Length", str(len(body)))
                connection.putheader("X-Tempo-Request-Id", request_id)
                connection.endheaders()
                connection.send(body[:1])
                wait_event(
                    logs,
                    lambda e: e.get("request_id") == request_id
                    and e.get("event") == "http.body_started"
                    and e.get("operation") == "http.read_body",
                )
                # Hold a real SQLite lock before allowing the request into the
                # application, then measure and correlate the storage wait.
                lock.execute("BEGIN EXCLUSIVE")
                started = time.monotonic()
                connection.send(body[1:])

                def receive():
                    response = connection.getresponse()
                    return response.status, response.headers, json.loads(response.read())

                with ThreadPoolExecutor(max_workers=1) as executor:
                    pending = executor.submit(receive)
                    stall = wait_event(
                        logs,
                        lambda e: e.get("request_id") == request_id
                        and e.get("event") == "operation.stalled"
                        and e.get("operation", "").startswith("sqlite."),
                        timeout=4.5,
                    )
                    self.assertGreaterEqual(stall["elapsed_ms"], 1000)
                    status, headers, reply = pending.result(timeout=7)
                elapsed = time.monotonic() - started
                self.assertGreaterEqual(elapsed, 4.5)
                self.assertLess(elapsed, 8)
                self.assertEqual(status, 200)  # Existing typed application-error envelope.
                self.assertEqual(headers["X-Tempo-Request-Id"], request_id)
                self.assertEqual(reply["code"], "store_unavailable")
            finally:
                lock.rollback()
                lock.close()
                connection.close()

            wait_event(
                logs,
                lambda e: e.get("request_id") == request_id
                and e.get("event") == "sqlite.error"
                and e.get("sqlite_code") in (5, 6),
            )
            slow = wait_event(
                logs,
                lambda e: e.get("request_id") == request_id
                and e.get("event") == "store.end"
                and e.get("elapsed_ms", 0) >= 4500,
            )
            self.assertEqual(slow["level"], "warn")
            self.assert_correlated(logs, request_id)
            status, _, payload, recovery_id = server.request()
            self.assertEqual(status, 200)
            self.assertEqual(json.loads(payload), baseline)
            self.assert_correlated(logs, recovery_id)


if __name__ == "__main__":
    unittest.main()
