#!/usr/bin/env python3
"""Prepare and deliberately apply explicit Tempo opening misses over local HTTP."""

import argparse
import contextlib
import fcntl
import http.client
import json
import os
from pathlib import Path
import random
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

CONTRACT = 1
BODY_LIMIT = 16 * 1024
MANIFEST_LIMIT = 1000
RETRY_DELAYS = (0.25, 0.5, 1, 2, 4)
MISSES = "/api/openings/misses"
GOALS = "/api/openings/introduction-priorities"
PAUSE_CARD = {"learning_changed", "eligibility_changed", "target_in_active_study",
              "target_changed", "authoring_changed"}
STOP_BATCH = {"stale_dataset", "idempotency_conflict"}


class ClientError(Exception):
    pass


class TransportError(ClientError):
    pass


class ReplyError(ClientError):
    def __init__(self, status, response):
        self.status = status
        self.response = response
        self.code = response.get("code")
        super().__init__(response.get("error", f"HTTP {status}"))


class LocalOnlyRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def encode(value):
    return (json.dumps(value, ensure_ascii=False, separators=(",", ":"), allow_nan=False)
            .encode("utf-8"))


class LocalTempo:
    def __init__(self, base):
        url = urllib.parse.urlsplit(base)
        if (url.scheme != "http" or url.hostname not in {"127.0.0.1", "localhost"}
                or url.port is None or url.username is not None or url.password is not None
                or url.path not in {"", "/"} or url.query or url.fragment):
            raise ClientError("Use an HTTP loopback address with an explicit port.")
        self.base = base.rstrip("/")
        self.http = urllib.request.build_opener(
            urllib.request.ProxyHandler({}), LocalOnlyRedirect())

    def request(self, route, body=None):
        data = None if body is None else encode(body)
        if data is not None and len(data) > BODY_LIMIT:
            raise ClientError("Request exceeds 16 KiB.")
        request = urllib.request.Request(
            self.base + route, data=data, headers={"Content-Type": "application/json"})
        try:
            with self.http.open(request, timeout=10) as reply:
                return self.decode(reply)
        except urllib.error.HTTPError as error:
            with error:
                try:
                    payload = self.decode(error)
                except TransportError:
                    payload = {}
            raise ReplyError(error.code, payload) from error
        except (OSError, EOFError, http.client.HTTPException) as error:
            raise TransportError(str(error)) from error

    @staticmethod
    def decode(reply):
        try:
            raw = reply.read(1024 * 1024 + 1)
            if len(raw) > 1024 * 1024:
                raise ValueError("response exceeds 1 MiB")
            value = json.loads(raw)
            if not isinstance(value, dict):
                raise ValueError("expected a JSON object")
            return value
        except (OSError, EOFError, ValueError, http.client.HTTPException) as error:
            raise TransportError(f"Unable to read Tempo's response: {error}") from error

    def dataset(self):
        value = self.request("/api/tactics/capture-context").get("dataset")
        if not isinstance(value, str) or not value:
            raise ClientError("Tempo returned no current dataset identity.")
        return value


def status_path(route, identity, dataset):
    return f"{route}/{urllib.parse.quote(str(identity), safe='')}?" + urllib.parse.urlencode(
        {"dataset": dataset})


def rows(path):
    with path.open("rb") as stream:
        for number, line in enumerate(stream, 1):
            if number > MANIFEST_LIMIT:
                raise ClientError("Split selections into at most 1,000 entries.")
            if not line.strip():
                continue
            try:
                row = json.loads(line)
                if not isinstance(row, dict):
                    raise ValueError("expected a JSON object")
            except ValueError as error:
                raise ClientError(f"Invalid selection at line {number}: {error}") from error
            yield row


def archive(source, selection):
    game_id, repertoire_id = selection["game_id"], selection["repertoire_id"]
    pair = [urllib.parse.quote(value, safe="") for value in (game_id, repertoire_id)]
    view = source.request(f"/api/games/archive/{pair[0]}/feedback/{pair[1]}")
    if not view.get("current") or not view.get("report"):
        raise ClientError("No current completed comparison. Request it in Games, then prepare again.")
    basis = view["completed"]
    differences = {}
    by_ply = {}
    for move in view["report"]["decisions"]:
        if move["outcome"] != "differed":
            continue
        position = move["position"]
        differences.setdefault(position, []).append(move["ply"])
        by_ply[move["ply"]] = position
    selected = set(differences)
    if "plies" in selection:
        plies = selection["plies"]
        if not plies or any(type(ply) is not int or ply not in by_ply for ply in plies):
            raise ClientError("Each selected ply must be a differing learner decision.")
        selected = {by_ply[ply] for ply in plies}
    for position in sorted(selected, key=lambda pos: (min(differences[pos]), pos)):
        yield {
            "event": ["archived_game", game_id, repertoire_id, position],
            "position_id": position,
            "source": {"kind": "archived_game", "game_id": game_id, "repertoire_id": repertoire_id,
                       "decision_plies": sorted(differences[position]),
                       "report_generation": basis["generation"],
                       "comparator_version": basis["comparator_version"],
                       "repertoire_revision": basis["repertoire_revision"]},
        }


def selected_events(tempo, selection):
    match selection.get("kind"):
        case "manual":
            event_id = str(uuid.UUID(selection["event_id"]))
            yield {"event": ["manual", event_id], "card_id": selection["card_id"],
                   "source": {"kind": "manual", "event_id": event_id}}
        case "archived_game":
            yield from archive(tempo, selection)
        case _:
            raise ClientError("Selection kind must be manual or archived_game.")


@contextlib.contextmanager
def lock(manifest):
    path = manifest.with_suffix(manifest.suffix + ".lock")
    with path.open("a+b") as stream:
        try:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise ClientError("Another process is using this manifest.") from error
        yield


def atomic_lines(path, values):
    with tempfile.NamedTemporaryFile(dir=path.parent, prefix=f".{path.name}.",
                                     delete=False) as stream:
        temporary = Path(stream.name)
        try:
            for value in values:
                stream.write(encode(value) + b"\n")
            stream.flush()
            os.fsync(stream.fileno())
            os.replace(temporary, path)
        finally:
            temporary.unlink(missing_ok=True)
    sync_directory(path.parent)


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def journal_path(manifest):
    return manifest.with_suffix(manifest.suffix + ".journal")


def prepare(tempo, selections, manifest):
    dataset = tempo.dataset()
    seen = set()
    entries = []
    with lock(manifest):
        if manifest.exists() or journal_path(manifest).exists():
            raise ClientError("Use a new manifest path to preserve the previous selections and journal.")
        for selection in rows(selections):
            try:
                for event in selected_events(tempo, selection):
                    key = tuple(event.pop("event"))
                    if key in seen:
                        continue
                    seen.add(key)
                    if len(seen) > MANIFEST_LIMIT:
                        raise ClientError("The selection yields more than 1,000 events. Split it.")
                    try:
                        preview = tempo.request(f"{MISSES}/preview", {
                            **event, "dataset": dataset, "request_id": str(uuid.uuid4()),
                            "provenance": selection.get("provenance", {}),
                        })
                        if preview.get("delivery") not in {"preview", "replayed", "already_processed"}:
                            raise TransportError("Tempo returned an unknown preview result.")
                        entries.append({"event": key, **preview})
                    except ReplyError as error:
                        if error.code in STOP_BATCH:
                            raise
                        entries.append({"event": key, "delivery": "rejected",
                                        "error": str(error), "code": error.code})
            except (KeyError, TypeError, ValueError) as error:
                entries.append({"delivery": "rejected", "selection": selection, "error": str(error)})
            except ClientError as error:
                if len(seen) > MANIFEST_LIMIT or isinstance(error, TransportError):
                    raise
                if isinstance(error, ReplyError) and error.code in STOP_BATCH:
                    raise
                entries.append({"delivery": "rejected", "selection": selection, "error": str(error)})
        if len(entries) > MANIFEST_LIMIT:
            raise ClientError("The selection yields more than 1,000 entries. Split it.")
        atomic_lines(manifest, [{"contract_version": CONTRACT, "dataset": dataset}, *entries])
    summary(entries, manifest)
    return 2 if any(item["delivery"] == "rejected" for item in entries) else 0


def summary(entries, manifest):
    targets = {}
    again = unseen = replayed = rejected = 0
    for item in entries:
        kind = item["delivery"]
        if kind == "rejected":
            rejected += 1
            print(f"Skipped {item.get('event', item.get('selection'))}: {item['error']}")
        elif kind == "preview":
            command = item["command"]
            card = command["target"]["card_id"]
            branch = command["expected"]["learning_state"]
            again += branch == "introduced"
            unseen += branch == "unseen"
            targets[card] = targets.get(card, 0) + 1
            priority = item.get("priority", {}).get("progress") or {}
            origin = (item.get("evidence") or {}).get("origin", {})
            date = origin.get("source_date") or "date unknown"
            print(f"{card}  {branch}  {date}  {priority.get('reason') or 'priority proof pending'}"
                  f"  {priority.get('next', [])}")
        else:
            replayed += 1
            print(f"Already processed {item['event']}  {item['receipt']['learning_effect']}")
    print(f"{len(entries)} entries, {len(targets)} unresolved targets, {again} Again, "
          f"{unseen} introduction only, {replayed} already processed, {rejected} skipped.")
    for card, count in targets.items():
        if count > 1:
            print(f"Multiple events for {card}: {count}")
    print(f"Review {manifest} before applying.")


def load_manifest(path):
    with path.open("rb") as stream:
        header = json.loads(stream.readline())
        if header.get("contract_version") != CONTRACT or not isinstance(header.get("dataset"), str):
            raise ClientError("Invalid manifest header.")
        entries = []
        for line in stream:
            if len(entries) == MANIFEST_LIMIT:
                raise ClientError("Manifest exceeds 1,000 entries.")
            entries.append(json.loads(line))
        return header["dataset"], entries


def append(stream, event):
    stream.write(encode(event) + b"\n")
    stream.flush()
    os.fsync(stream.fileno())


def read_journal(stream, entries):
    stream.seek(0)
    finalized, outcomes = {}, {}
    pending = None
    latest = -1
    versions = {}
    for line in stream:
        if not line.endswith(b"\n"):
            stream.truncate(stream.tell() - len(line))
            stream.flush()
            os.fsync(stream.fileno())
            break
        event = json.loads(line)
        index = event["index"]
        if type(index) is not int or not 0 <= index < len(entries) or index in outcomes:
            raise ClientError("Journal has an invalid entry identity or ordering.")
        if event["type"] == "finalized" and pending is None and index > latest:
            original = entries[index]["command"]
            command = event["command"]
            card = original["target"]["card_id"]
            expected = {**original["expected"], "schedule_version": versions.get(
                card, original["expected"]["schedule_version"])}
            if {**original, "expected": expected} != command:
                raise ClientError("Finalized request disagrees with the manifest.")
            finalized[index] = command
            pending = latest = index
        elif event["type"] == "result" and index == pending:
            reply = event["outcome"]
            if reply.get("delivery") == "rejected":
                if not isinstance(reply.get("error"), str) or "code" not in reply:
                    raise ClientError("Invalid rejection in the journal.")
            else:
                check_delivery(reply, finalized[index])
                update_chain(versions, finalized[index]["target"]["card_id"], reply)
            outcomes[index] = reply
            pending = None
        else:
            raise ClientError("Journal contains an invalid transition.")
    stream.seek(0, os.SEEK_END)
    return finalized, outcomes


def check_delivery(reply, command):
    delivery = reply.get("delivery")
    receipt = reply.get("receipt")
    if (delivery not in {"accepted", "replayed", "already_processed"}
            or not isinstance(receipt, dict) or receipt.get("dataset") != command["dataset"]
            or reply.get("request_id") != command["request_id"]
            or (delivery != "already_processed" and receipt.get("request_id") != command["request_id"])):
        raise TransportError("Tempo returned an unrecognized or mismatched acceptance.")
    source = command["source"]
    if source["kind"] == "manual":
        event = [command["dataset"], "manual", source["event_id"].lower()]
    else:
        event = [command["dataset"], "archived_game", source["game_id"],
                 source["repertoire_id"], command["target"]["position_id"]]
    if receipt.get("event") != event:
        raise TransportError("Tempo returned a receipt for a different event.")
    if delivery != "already_processed":
        effect = "again" if command["expected"]["learning_state"] == "introduced" else "none_unseen"
        if receipt.get("target") != command["target"] or receipt.get("learning_effect") != effect:
            raise TransportError("Tempo returned an unexpected target or effect.")
        if effect == "again":
            try:
                version = receipt["rating"]["after"]["schedule_version"]
            except (KeyError, TypeError) as error:
                raise TransportError("The Again receipt lacks its schedule version.") from error
            if type(version) is not int or version <= command["expected"]["schedule_version"]:
                raise TransportError("The Again receipt has an invalid schedule version.")
    return reply


def update_chain(versions, card, reply):
    receipt = reply["receipt"]
    if reply["delivery"] in {"accepted", "replayed"} and receipt["learning_effect"] == "again":
        versions[card] = receipt["rating"]["after"]["schedule_version"]


def submit(tempo, command):
    for delay in (0, *RETRY_DELAYS):
        if delay:
            time.sleep(delay + random.uniform(0, 0.05))
        try:
            return check_delivery(tempo.request(MISSES, command), command)
        except ReplyError as error:
            if error.status < 500:
                if error.code in STOP_BATCH:
                    raise
                return {"delivery": "rejected", "error": str(error), "code": error.code}
        except TransportError:
            pass
    try:
        route = status_path(f"{MISSES}/receipts", command["request_id"], command["dataset"])
        return check_delivery(tempo.request(route), command)
    except (ReplyError, TransportError) as error:
        if isinstance(error, ReplyError) and error.code in STOP_BATCH:
            raise
        return None


def apply(tempo, manifest):
    with lock(manifest):
        dataset, entries = load_manifest(manifest)
        if dataset != tempo.dataset():
            print("Stopped: this manifest belongs to another Tempo dataset.")
            return 3
        journal = journal_path(manifest)
        new_journal = not journal.exists()
        with journal.open("a+b") as stream:
            if new_journal:
                sync_directory(journal.parent)
            finalized, outcomes = read_journal(stream, entries)
            return apply_entries(tempo, stream, journal, entries, finalized, outcomes)


def apply_entries(tempo, stream, journal, entries, finalized, outcomes):
    versions = {}
    paused = set()
    succeeded = rejected = skipped = 0
    for index, item in enumerate(entries):
        if item["delivery"] != "preview":
            rejected += item["delivery"] == "rejected"
            succeeded += item["delivery"] in {"already_processed", "replayed"}
            continue
        original = item["command"]
        card = original["target"]["card_id"]
        if index in outcomes:
            reply = outcomes[index]
        else:
            if card in paused and index not in finalized:
                skipped += 1
                continue
            command = finalized.get(index)
            if command is None:
                expected = {**original["expected"]}
                if card in versions:
                    expected["schedule_version"] = versions[card]
                command = {**original, "expected": expected}
                append(stream, {"type": "finalized", "index": index, "command": command})
            try:
                reply = submit(tempo, command)
            except ReplyError as error:
                print(f"Stopped at item {index + 1}: {error}. Journal {journal}")
                return 3
            if reply is None:
                print(f"Stopped with unknown outcome at item {index + 1}. Resume with {journal}.")
                return 3
            append(stream, {"type": "result", "index": index, "outcome": reply})
        if reply["delivery"] == "rejected":
            rejected += 1
            if reply["code"] in PAUSE_CARD:
                paused.add(card)
            print(f"Rejected item {index + 1} for {card}: {reply['error']}")
            continue
        succeeded += 1
        update_chain(versions, card, reply)
    print(f"{succeeded} resolved, {rejected} rejected, {skipped} deferred for fresh preparation."
          f" Journal {journal}")
    return 2 if rejected or skipped else 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", required=True, help="Tempo's loopback HTTP address")
    sub = parser.add_subparsers(dest="operation", required=True)
    build = sub.add_parser("prepare", help="preview explicit selections")
    build.add_argument("selections", type=Path)
    build.add_argument("manifest", type=Path)
    run = sub.add_parser("apply", help="apply exactly the selections in a prepared manifest")
    run.add_argument("manifest", type=Path)
    status = sub.add_parser("status", help="read an accepted receipt or current goal")
    identity = status.add_mutually_exclusive_group(required=True)
    identity.add_argument("--request")
    identity.add_argument("--goal", type=int)
    cancel = sub.add_parser("cancel-priority", help="withdraw the goal's uncommitted priority")
    cancel.add_argument("goal", type=int)
    args = parser.parse_args(argv)
    try:
        tempo = LocalTempo(args.url)
        if args.operation == "prepare":
            return prepare(tempo, args.selections, args.manifest)
        if args.operation == "apply":
            return apply(tempo, args.manifest)
        dataset = tempo.dataset()
        if args.operation == "cancel-priority":
            reply = tempo.request(f"{GOALS}/{args.goal}/cancel", {"dataset": dataset})
        elif args.request:
            reply = tempo.request(status_path(f"{MISSES}/receipts", args.request, dataset))
        else:
            reply = tempo.request(status_path(GOALS, args.goal, dataset))
        print(json.dumps(reply, indent=2, ensure_ascii=False))
        return 0
    except (ClientError, OSError, ValueError, KeyError, TypeError) as error:
        print(f"Opening misses: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
