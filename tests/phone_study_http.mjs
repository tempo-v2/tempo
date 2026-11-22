// Run against a built candidate with TEMPO_BINARY=/absolute/path/to/tempo.
// The phone ingress admits the same Study commands while retaining its explicit
// route, owner, origin and method boundaries when desktop features are added.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { request } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import test from "node:test";
import { DatabaseSync } from "node:sqlite";
import { databaseRows } from "./support/game_detection_readback.mjs";

const binary = resolve(process.env.TEMPO_BINARY || "target/debug/tempo");
const phoneOrigin = "https://phone-fixture.ts.net:8443";
const phoneAuthority = new URL(phoneOrigin).host;
const phoneHeaders = {
  Host: phoneAuthority,
  "X-Forwarded-Host": phoneAuthority,
  "X-Forwarded-Proto": "https",
  "Tailscale-User-Login": "owner@example.invalid",
  Origin: phoneOrigin,
};

async function start(database, ui, phoneEnabled) {
  const env = { ...process.env };
  delete env.TEMPO_PHONE_ORIGIN;
  delete env.TEMPO_PHONE_TAILSCALE_LOGIN;
  if (phoneEnabled) Object.assign(env, {
    TEMPO_PHONE_ORIGIN: phoneOrigin,
    TEMPO_PHONE_TAILSCALE_LOGIN: phoneHeaders["Tailscale-User-Login"],
  });
  const child = spawn(binary, ["serve", database, ui, "0"], { env });
  const output = createInterface({ input: child.stdout });
  let diagnostics = "";
  child.stderr.on("data", chunk => { diagnostics += chunk; });
  for await (const line of output) {
    const match = /^Tempo study at (http:\/\/127\.0\.0\.1:\d+)$/.exec(line);
    if (match) return {
      origin: match[1],
      async stop() {
        if (child.exitCode !== null || child.signalCode !== null) return;
        const stopped = once(child, "exit");
        child.kill();
        await stopped;
      },
    };
  }
  assert.fail(diagnostics || "Tempo did not start");
}

function send(host, route, { method = "GET", headers = {}, payload } = {}) {
  return new Promise((resolveReply, reject) => {
    const bytes = payload === undefined ? undefined : JSON.stringify(payload);
    const req = request(host.origin + route, {
      method, headers: { "Content-Type": "application/json", ...headers }, timeout: 5000,
    }, response => {
      let body = "";
      response.setEncoding("utf8");
      response.on("data", chunk => { body += chunk; });
      response.on("end", () => resolveReply({ status: response.statusCode, body }));
      response.on("error", reject);
    });
    req.on("error", reject);
    req.on("timeout", () => req.destroy(new Error("HTTP fixture timed out")));
    if (headers.Expect === "100-continue") {
      req.once("continue", () => req.end(bytes));
      req.flushHeaders();
    } else {
      req.end(bytes);
    }
  });
}

test("phone Study retains its shared commands and denies desktop routes after integration", { timeout: 15000 }, async t => {
  const directory = await mkdtemp(join(tmpdir(), "tempo-phone-ingress-"));
  const database = join(directory, "study.sqlite");
  const ui = join(directory, "ui");
  let host;
  t.after(async () => {
    await host?.stop();
    await rm(directory, { recursive: true, force: true });
  });
  for (const surface of [ui, join(ui, "phone")]) {
    await mkdir(join(surface, "assets"), { recursive: true });
    await writeFile(join(surface, "index.html"), surface === ui ? "Desktop fixture" : "Phone fixture");
  }
  execFileSync(binary, ["init", database]);
  host = await start(database, ui, true);
  const post = async (route, payload, headers = {}) => {
    const reply = await send(host, route, { method: "POST", headers, payload });
    assert.equal(reply.status, 200, reply.body);
    return JSON.parse(reply.body);
  };
  assert.deepEqual(await send(host, "/"), { status: 200, body: "Desktop fixture" });
  assert.deepEqual(await send(host, "/", { headers: phoneHeaders }), { status: 200, body: "Phone fixture" });
  const desktop = await post("/api/study-progress", null);
  assert.equal(typeof desktop.stamp?.dataset_id, "string", JSON.stringify(desktop));
  assert.deepEqual(await post("/api/study-progress", null, phoneHeaders), desktop);
  const sql = new DatabaseSync(database, { timeout: 5000 });
  const { dataset_id: contextDataset, policy } = sql.prepare(
    "SELECT dataset_id, policy FROM store_state WHERE singleton=1"
  ).get();
  assert.equal(desktop.stamp.dataset_id, contextDataset);
  const { user_version } = sql.prepare("PRAGMA user_version").get();
  try {
    sql.prepare("UPDATE store_state SET policy='not-json' WHERE singleton=1").run();
    const expected = { status: "ready", dataset_id: contextDataset, schema_version: user_version };
    for (const headers of [{}, phoneHeaders]) {
      const context = await send(host, "/api/context", { headers });
      assert.equal(context.status, 200, context.body);
      assert.deepEqual(JSON.parse(context.body), expected);
    }
    assert.equal((await post("/api/study", { command: "study_status" })).status, "error");
    assert.equal((await send(host, "/api/context", {
      headers: { ...phoneHeaders, "Tailscale-User-Login": "other@example.invalid" },
    })).status, 403);
  } finally {
    sql.prepare("UPDATE store_state SET policy=? WHERE singleton=1").run(policy);
    sql.close();
  }

  const retiredDetector = await send(host, "/api/games/detectors");
  assert.equal(retiredDetector.status, 410);
  assert.equal(JSON.parse(retiredDetector.body).code, "retired_feature");
  assert.equal((await send(host, "/api/games")).status, 200);

  // Reject oversized disposable input before decoding its command.
  const beforeAdmission = databaseRows(database);
  const oversizedPgn = { command: "save", pgn: " ".repeat(2 * 1024 * 1024) };
  for (const route of ["/api/repertoires", "/api/tactics", "/api/games/import"]) {
    const reply = await send(host, route, {
      method: "POST",
      payload: oversizedPgn,
      headers: {
        Expect: "100-continue",
        "Content-Length": Buffer.byteLength(JSON.stringify(oversizedPgn)),
      },
    });
    assert.equal(reply.status, 413, reply.body);
    const error = JSON.parse(reply.body);
    assert.equal(error.code, "invalid_command");
    assert.equal(error.retryable, false);
    assert.match(error.message, /2048 KiB/);
  }
  assert.deepEqual(databaseRows(database), beforeAdmission);
  const before = databaseRows(database);
  const presence = { tab: "7a791296-6215-4d60-ae7c-58bab00ce975", sequence: 1, state: "active" };
  const retiredPresence = await send(host, "/api/study/presence", {
    method: "POST", headers: phoneHeaders, payload: presence,
  });
  assert.equal(retiredPresence.status, 410);
  assert.equal(JSON.parse(retiredPresence.body).code, "retired_feature");
  assert.equal((await send(host, "/api/study/presence", {
    method: "POST", headers: { ...phoneHeaders, Origin: "https://another.ts.net:8443" }, payload: presence,
  })).status, 403);
  for (const [route, method] of [
    ["/api/games/detectors", "GET"], ["/api/games/detection", "POST"],
    ["/api/games/detection-result", "POST"], ["/api/games/library", "POST"],
    ["/api/games/review", "POST"], ["/api/games/import", "POST"],
    ["/api/games/job", "GET"], ["/api/games/refresh", "POST"],
    ["/api/repertoires", "GET"], ["/api/study/analysis-context", "POST"],
    ["/api/games/model", "POST"], ["/api/games/gaps", "GET"],
    ["/api/games/tactics", "POST"], ["/api/games/tactics/command", "POST"],
    ["/api/games/gap-analysis", "GET"], ["/api/games/gap-explorer", "POST"],
    ["/api/study", "PUT"], ["/api/study", "GET"], ["/api/study/presence", "GET"],
  ]) assert.equal((await send(host, route, { method, headers: phoneHeaders, payload: {} })).status, 404, route);
  for (const path of [
    "/api/games/gap-analysis", "/api/games/gap-analysis/status", "/api/games/gap-line", "/api/games/gap-line/status",
  ]) {
    const retired = await send(host, path, { method: "POST", headers: phoneHeaders, payload: {} });
    assert.equal(retired.status, 410);
    assert.equal(JSON.parse(retired.body).code, "retired_feature");
  }
  for (const changes of [
    { Host: "another.ts.net:8443" }, { "X-Forwarded-Host": "another.ts.net:8443" },
    { "X-Forwarded-Proto": "http" }, { "Tailscale-User-Login": "other@example.invalid" },
    { Origin: "https://another.ts.net:8443" }, { "Tailscale-Funnel-Request": "1" },
  ]) assert.equal((await send(host, "/api/study", {
    method: "POST", headers: { ...phoneHeaders, ...changes }, payload: { command: "next" },
  })).status, 403);
  const noOrigin = { ...phoneHeaders };
  delete noOrigin.Origin;
  assert.equal((await send(host, "/api/study", {
    method: "POST", headers: noOrigin, payload: { command: "next" },
  })).status, 403);
  assert.deepEqual(databaseRows(database), before);

  const { dataset_id } = await post("/api/repertoires", { command: "list" });
  assert.equal((await post("/api/repertoires", { command: "list" }, phoneHeaders)).dataset_id, dataset_id);
  assert.equal((await post("/api/repertoires", {
    command: "preview", dataset_id, name: "Unsupported", learner_color: "white", pgn: "1. d4 *",
  }, phoneHeaders)).code, "phone_build_only");
  for (const headers of [{ ...phoneHeaders, Origin: "https://elsewhere.ts.net:8443" },
    { ...phoneHeaders, "Tailscale-User-Login": "someone@example.invalid" }, noOrigin]) {
    const denied = await send(host, "/api/repertoires", { method: "POST", headers, payload: { command: "list" } });
    assert.equal(denied.status, 403);
  }
  const globalGaps = await send(host, "/api/games/gaps", {
    method: "POST", headers: phoneHeaders, payload: { offset: 0 },
  });
  assert.equal(globalGaps.status, 400);
  assert.equal(JSON.parse(globalGaps.body).code, "phone_build_only");
  const input = { dataset_id, name: "Ingress fixture", learner_color: "white", pgn: "1. e4 *" };
  const preview = await post("/api/repertoires", { ...input, command: "preview" });
  const save = { ...input, command: "save", token: preview.token };
  assert.equal((await post("/api/repertoires", save, phoneHeaders)).code, "phone_build_only");
  const { repertoires: [{ id: repertoire_id }] } = await post("/api/repertoires", save);
  for (const command of [
    { command: "read_document" },
    { command: "rename", name: "Changed on phone" },
    { command: "set_activity", command_id: "phone-suspend", activity: "suspended" },
    { command: "reset_progress", command_id: "phone-reset" },
    { ...input, command: "edit_preview", base_revision: 1 },
    { command: "marked_preview", name: input.name, learner_color: "white", anchor_fen: "start", lines: [] },
  ]) assert.equal((await post("/api/repertoires", { ...command, dataset_id, repertoire_id }, phoneHeaders)).code,
    "phone_build_only", command.command);
  const scopedGaps = await post("/api/games/gaps", { offset: 0, forecast_repertoire: repertoire_id }, phoneHeaders);
  assert.deepEqual(scopedGaps.gaps, []);
  for (const path of ["/api/games/coverage", "/api/games/coverage/status"]) {
    const retired = await send(host, path, { method: "POST", headers: phoneHeaders, payload: { dataset_id, repertoire_id } });
    assert.equal(retired.status, 410);
    assert.equal(JSON.parse(retired.body).code, "retired_feature");
  }
  const issued = await post("/api/study", { command: "next" }, phoneHeaders);
  const resumeSql = new DatabaseSync(database, { timeout: 5000 });
  const { state: learningBeforeResume } = resumeSql.prepare(
    "SELECT state FROM learning WHERE target_id=?"
  ).get(issued.target_id);
  const { item: issuedItem, version: issuedVersion } = resumeSql.prepare(
    "SELECT h.item,h.version FROM content_history h JOIN current_content c USING(dataset_id,target_id,version) WHERE h.target_id=?"
  ).get(issued.target_id);
  let resumed;
  try {
    resumeSql.prepare("UPDATE learning SET state='not-json' WHERE target_id=?").run(issued.target_id);
    resumeSql.prepare(
      "UPDATE content_history SET item=json_set(item,'$.verified_tactic',json('{}')) WHERE dataset_id=? AND target_id=? AND version=?"
    ).run(dataset_id, issued.target_id, issuedVersion);

    resumed = await post("/api/study", { command: "resume", attempt_id: issued.attempt_id });
    assert.equal(resumed.status, "ready");
  } finally {
    resumeSql.prepare("UPDATE learning SET state=? WHERE target_id=?").run(learningBeforeResume, issued.target_id);
    resumeSql.prepare(
      "UPDATE content_history SET item=? WHERE dataset_id=? AND target_id=? AND version=?"
    ).run(issuedItem, dataset_id, issued.target_id, issuedVersion);

    resumeSql.close();
  }
  assert.equal(resumed.attempt_id, issued.attempt_id);
  assert.equal(resumed.dataset_id, dataset_id);
  const command = {
    command: "acknowledge", dataset_id, attempt_id: issued.attempt_id,
    expected_revision: issued.revision, submission_id: "phone-shared-ack",
    answer: { uci: issued.lesson.move_uci },
  };
  const accepted = await post("/api/study", command, phoneHeaders);
  assert.equal(accepted.status, "complete");
  const after = databaseRows(database);
  assert.deepEqual(await post("/api/study", command), accepted);
  assert.deepEqual(databaseRows(database), after);

  await host.stop();
  host = await start(database, ui, false);
  assert.equal((await send(host, "/", { headers: phoneHeaders })).status, 403);
  assert.equal((await send(host, "/", { headers: { "X-Forwarded-Host": new URL(host.origin).host } })).status, 403);
  assert.deepEqual(await send(host, "/"), { status: 200, body: "Desktop fixture" });
});
