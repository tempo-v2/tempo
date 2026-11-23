"""Exercise the installer probe against a disposable HTTP endpoint."""

import contextlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
from threading import Thread
import unittest

from managed_start import context_status


@contextlib.contextmanager
def context_endpoint(payload):
    paths = []

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            paths.append(self.path)
            self.send_response(200)
            self.end_headers()
            self.wfile.write(json.dumps(payload).encode())

        def log_message(self, *_args):
            pass

    with HTTPServer(("127.0.0.1", 0), Handler) as server:
        thread = Thread(target=server.serve_forever)
        thread.start()
        try:
            yield server.server_port, paths
        finally:
            server.shutdown()
            thread.join()


class ManagedContextTests(unittest.TestCase):
    def test_readiness_is_independent_of_study(self):
        with context_endpoint({
            "status": "ready", "dataset_id": "fixture", "schema_version": 22,
            "study": "preparing",
        }) as (port, paths):
            context_status(port, "fixture", 22)
            self.assertEqual(paths, ["/api/context"])

    def test_rejects_wrong_dataset_schema_or_unready_storage(self):
        expected = {"status": "ready", "dataset_id": "fixture", "schema_version": 22}
        for changes in (
            {"dataset_id": "different"}, {"schema_version": 21}, {"status": "unavailable"},
        ):
            with self.subTest(changes=changes):
                with context_endpoint({**expected, **changes}) as (port, _):
                    with self.assertRaisesRegex(ValueError, "schema readiness"):
                        context_status(port, "fixture", 22)


if __name__ == "__main__":
    unittest.main()
