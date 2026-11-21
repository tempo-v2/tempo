"""Behavior checks for the read-only host diagnostic export."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

MODULE_PATH = Path(__file__).resolve().parents[1] / "diagnose.py"
SPEC = importlib.util.spec_from_file_location("diagnose", MODULE_PATH)
diagnose = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(diagnose)


class Processes(unittest.TestCase):
    def test_real_ps_columns_keep_known_label_and_exact_pid_outside_top_twenty(self):
        text = "12 1 0.1 4096 /Users/private/project/tempo\n"
        text += "15 1 14.5 600 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome\n"
        text += "\n".join(f"{100+i} 1 {80-i}.5 100 /private/compiler/rustc" for i in range(25))
        actual = diagnose.parse_processes(text, 12)
        self.assertEqual(len(actual["top_processes"]), 20)
        self.assertEqual(actual["top_processes"][0]["kind_hint"], "compiler_or_build_tool")
        self.assertEqual(actual["observed_process"]["executable"], "tempo")
        self.assertEqual(actual["observed_process"]["rss_bytes"], 4096 * 1024)
        self.assertNotIn("/Users", json.dumps(actual))
        chrome = diagnose.parse_processes(text, 15)["observed_process"]
        self.assertEqual(chrome["executable"], "google chrome")
        self.assertEqual(chrome["kind_hint"], "browser")
        self.assertNotIn(".app", json.dumps(chrome))

    def test_private_executable_names_and_engine_suffixes_are_redacted(self):
        text = ("12 7 25.0 4096 /private/path/secret-tool\n"
                "13 7 20.0 100 /private/path/stockfish-secret-build\n"
                "14 7 5.0 100 /private/path/python3.11\n")
        export = diagnose.parse_processes(text, 12)
        self.assertEqual(
            [(row["pid"], row["executable"], row["kind_hint"]) for row in export["top_processes"]],
            [(12, "other", "other"), (13, "stockfish", "chess_engine"),
             (14, "python", "python_runtime")],
        )
        self.assertEqual(export["observed_process"]["parent_pid"], 7)
        self.assertNotIn("secret", json.dumps(export))
        self.assertNotIn("/private/path", json.dumps(export))
        self.assertNotIn("python3.11", json.dumps(export))

    def test_invalid_process_fields_do_not_poison_json(self):
        actual = diagnose.parse_processes("1 0 nan 3 bad\n2 1 inf 4 bad\n-2 0 0 3 bad\n3 1 1 -1 bad\n4 1 2 5 stockfish")
        self.assertEqual(actual["processes_seen"], 1)
        self.assertEqual(actual["top_processes"][0]["kind_hint"], "chess_engine")

    def test_command_unavailable_is_evidence(self):
        self.assertEqual(diagnose.bounded_command(["/no/such/tempo-diagnostic-tool"]), ("", "unavailable"))

    def test_hanging_collector_child_has_deadline(self):
        started = time.monotonic()
        _, failure = diagnose.bounded_command([sys.executable, "-c", "import time; time.sleep(30)"], timeout=0.1)
        self.assertEqual(failure, "timeout")
        self.assertLess(time.monotonic() - started, 2)

    def test_output_budget_stops_owned_child(self):
        output, failure = diagnose.bounded_command([sys.executable, "-c", "print('x'*100000)"], max_bytes=64)
        self.assertEqual(failure, "output_limit")
        self.assertLessEqual(len(output), 64)


class Memory(unittest.TestCase):
    def test_linux_fields_have_exact_units(self):
        actual = diagnose.parse_linux_memory("MemTotal: 1000 kB\nMemAvailable: 250 kB\nSwapTotal: 200 kB\nSwapFree: 50 kB\nSecret: 123 kB")
        self.assertEqual(actual, {"total_bytes": 1024000, "available_bytes": 256000,
                                  "swap_total_bytes": 204800, "swap_free_bytes": 51200})

    def test_mac_uses_reported_page_size_and_distinguishes_counters(self):
        actual = diagnose.parse_macos_memory("Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free: 42.\nPages occupied by compressor: 3.\nSwapins: 5.\nSwapouts: 6.")
        self.assertEqual(actual["free_bytes"], 42 * 16384)
        self.assertEqual(actual["compressor_bytes"], 3 * 16384)
        self.assertEqual(actual["swapout_pages_total"], 6)


class LogExport(unittest.TestCase):
    def test_allowlist_drops_content_paths_messages_nested_values(self):
        actual = diagnose.sanitized_log_event({
            "schema_version": 1, "event": "request.end", "request_id": "diag-abc-4",
            "operation": "application.daily", "elapsed_ms": 5001.1, "active_spans": 2,
            "provider": "lichess", "run_id": "run-12", "route": "study",
            "pgn": "1.e4", "fen": "SECRET", "message": "secret /Users/private",
            "url": "https://user:secret@example.test?token=private", "body": {"secret": "value"},
            "request_bytes": 100, "thread": "4", "request_id2": "secret",
            "peak_rss_bytes": 98304, "rss_bytes": 65536, "queue_depth": 2, "count": 10,
            "listen_port": 41822,
            "error_code": "database_busy", "raw": "secret", "timestamp_ms": float("nan"),
        })
        encoded = json.dumps(actual, allow_nan=False)
        self.assertNotIn("secret", encoded)
        self.assertNotIn("timestamp_ms", actual)
        self.assertEqual(actual["active_spans"], 2)
        self.assertEqual(actual["error_code"], "database_busy")
        self.assertEqual(actual["provider"], "lichess")
        self.assertEqual(actual["peak_rss_bytes"], 98304)
        self.assertEqual(actual["rss_bytes"], 65536)
        self.assertEqual(actual["queue_depth"], 2)
        self.assertEqual(actual["count"], 10)
        self.assertEqual(actual["listen_port"], 41822)
        self.assertIsNone(diagnose.sanitized_log_event({"schema_version": 2, "event": "new.schema"}))
        self.assertIsNone(diagnose.sanitized_log_event({"schema_version": True, "event": "new.schema"}))

    def test_bad_types_and_extreme_numbers_are_ignored(self):
        actual = diagnose.sanitized_log_event({"schema_version": 1, "event": "span.end",
                                               "elapsed_ms": 10 ** 1000, "request_id": ["token"],
                                               "provider": {"token": "secret"}, "pid": True})
        self.assertEqual(actual, {"schema_version": 1, "event": "span.end"})

    def test_bounded_log_tail_skips_symlinks_unrelated_and_invalid_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            secret = root / "data.sqlite"
            secret.write_text("private content")
            (root / "tempo-link.jsonl").symlink_to(secret)
            (root / "unrelated.jsonl").write_text('{"schema_version":1,"event":"private"}\n')
            good = root / "tempo-run12.jsonl"
            events = [{"schema_version": 1, "event": "request.end", "span_id": str(i),
                       "body": "private content"} for i in range(20)]
            good.write_text("not json\n" + "\n".join(json.dumps(e) for e in events) + "\n")
            with patch.object(diagnose, "MAX_LOG_TAIL", 900), patch.object(diagnose, "MAX_LOG_EVENTS", 4):
                bundle = diagnose.collect_logs(root)
            self.assertEqual(bundle["files_read"], 1)
            self.assertEqual([e["span_id"] for e in bundle["events"]], ["16", "17", "18", "19"])
            self.assertEqual(bundle["truncated_file_tails"], 1)
            self.assertGreater(bundle["truncated_events"], 0)
            self.assertNotIn("private", json.dumps(bundle))
            self.assertEqual(diagnose.collect_logs(root / "missing")["status"], "unavailable")

    def test_rotated_names_and_file_count_budget(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for i in range(7):
                (root / f"tempo-run{i}.jsonl.1").write_text('{"schema_version":1,"event":"span.end"}\n')
            bundle = diagnose.collect_logs(root)
            self.assertEqual(bundle["files_read"], 5)
            self.assertEqual(bundle["omitted_matching_files"], 2)

    def test_span_ids_retain_exact_u64_precision_as_strings(self):
        first = diagnose.sanitized_log_event({"schema_version": 1, "event": "store.start",
                                              "span_id": "9223372036854775809",
                                              "parent_span_id": "9223372036854775808",
                                              "store_span_id": 1})
        second = diagnose.sanitized_log_event({"schema_version": 1, "event": "store.start",
                                               "span_id": "9223372036854775810"})
        self.assertEqual(first["span_id"], "9223372036854775809")
        self.assertEqual(first["parent_span_id"], "9223372036854775808")
        self.assertNotEqual(first["span_id"], second["span_id"])
        rejected = diagnose.sanitized_log_event({"schema_version": 1, "event": "store.start",
                                                "span_id": "private-content",
                                                "parent_span_id": "1" * 21})
        self.assertNotIn("span_id", rejected)
        self.assertNotIn("parent_span_id", rejected)


class Capture(unittest.TestCase):
    def test_interrupt_preserves_completed_samples(self):
        with patch.object(diagnose, "host_sample", return_value={"timestamp": "now"}), \
                patch.object(diagnose.time, "sleep", side_effect=KeyboardInterrupt):
            result = diagnose.collect(30, 2, 9, None)
        self.assertTrue(result["interrupted"])
        self.assertEqual(len(result["samples"]), 1)
        self.assertEqual(result["observed_pid"], 9)
        self.assertEqual(result["diagnostic_logs"]["status"], "not_requested")

    def test_cli_rejects_overwrite_before_sampling(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "existing.json"
            output.write_text("preserve")
            result = subprocess.run([sys.executable, str(MODULE_PATH), "--output", str(output)],
                                    capture_output=True, text=True, timeout=3)
            self.assertEqual(result.returncode, 2)
            self.assertEqual(output.read_text(), "preserve")
            self.assertNotIn("Collecting", result.stdout)

    def test_cli_rejects_unbounded_durations_and_nan(self):
        for number in ["nan", "inf", "0", "301"]:
            result = subprocess.run([sys.executable, str(MODULE_PATH), "--duration", number],
                                    capture_output=True, text=True, timeout=3)
            self.assertEqual(result.returncode, 2)


if __name__ == "__main__":
    unittest.main()
