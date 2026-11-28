import tempfile
from pathlib import Path
import unittest

import opening_miss as client


def command(index, version=4, state="introduced"):
    event = f"87654321-1234-1234-1234-{index:012}"
    return {
        "contract_version": 1,
        "dataset": "data",
        "request_id": f"12345678-1234-1234-1234-{index:012}",
        "source": {"kind": "manual", "event_id": event},
        "target": {"card_id": "card"},
        "expected": {"schedule_version": version, "learning_state": state},
    }


def delivery(payload, kind, next_version=5):
    receipt = {
        "request_id": payload["request_id"],
        "event": [payload["dataset"], "manual", payload["source"]["event_id"]],
        "dataset": payload["dataset"],
        "target": payload["target"],
        "learning_effect": "again" if payload["expected"]["learning_state"] == "introduced"
        else "none_unseen",
        "rating": {"after": {"schedule_version": next_version}},
    }
    return {"request_id": payload["request_id"], "delivery": kind, "receipt": receipt}


class JournalTests(unittest.TestCase):
    def test_torn_result_replays_exact_payload_then_chains_next_unsent(self):
        first, second = command(1), command(2)
        entries = [{"delivery": "preview", "command": item} for item in [first, second]]

        class Server:
            def __init__(self):
                self.sent = []

            def request(self, route, payload):
                self.sent.append(payload)
                if len(self.sent) == 1:
                    return delivery(payload, "replayed")
                return delivery(payload, "accepted", next_version=6)

        with tempfile.TemporaryDirectory() as directory:
            journal = Path(directory) / "batch.journal"
            with journal.open("a+b") as stream:
                client.append(stream, {"type": "finalized", "index": 0, "command": first})
                stream.write(b'{"type":"result","index":0')
                stream.flush()
                finalized, outcomes = client.read_journal(stream, entries)
                self.assertEqual({0: first}, finalized)
                self.assertEqual({}, outcomes)
                server = Server()
                self.assertEqual(0, client.apply_entries(
                    server, stream, journal, entries, finalized, outcomes))
                self.assertEqual(first, server.sent[0])
                self.assertEqual(5, server.sent[1]["expected"]["schedule_version"])
                finalized, outcomes = client.read_journal(stream, entries)
                self.assertEqual("replayed", outcomes[0]["delivery"])
                self.assertEqual("accepted", outcomes[1]["delivery"])

    def test_old_duplicate_and_unseen_receipts_do_not_update_version(self):
        versions = {"card": 9}
        duplicate = command(2)
        client.update_chain(versions, "card", delivery(duplicate, "already_processed"))
        client.update_chain(versions, "card", delivery(command(3, state="unseen"), "accepted"))
        self.assertEqual({"card": 9}, versions)

    def test_detects_second_unresolved_delivery_and_mismatched_receipt(self):
        commands = [command(1), command(2)]
        entries = [{"delivery": "preview", "command": item} for item in commands]
        with tempfile.TemporaryFile() as stream:
            for index, payload in enumerate(commands):
                client.append(stream, {"type": "finalized", "index": index, "command": payload})
            with self.assertRaises(client.ClientError):
                client.read_journal(stream, entries)
        with self.assertRaises(client.TransportError):
            client.check_delivery(delivery(commands[0], "replayed"), commands[1])


if __name__ == "__main__":
    unittest.main()
