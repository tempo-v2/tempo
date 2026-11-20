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
    req.end(bytes);
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
  assert.deepEqual(await post("/api/study-progress", null, phoneHeaders), desktop);
  assert.equal((await send(host, "/api/games/detectors")).status, 200);
  assert.equal((await send(host, "/api/games")).status, 200);

  const before = databaseRows(database);
  const presence = { tab: "7a791296-6215-4d60-ae7c-58bab00ce975", sequence: 1, state: "active" };
  assert.deepEqual(await send(host, "/api/study/presence", {
    method: "POST", headers: phoneHeaders, payload: presence,
  }), { status: 204, body: "" });
  assert.equal((await send(host, "/api/study/presence", {
    method: "POST", headers: phoneHeaders, payload: { ...presence, sequence: 0 },
  })).status, 400);
  assert.equal((await send(host, "/api/study/presence", {
    method: "POST", headers: { ...phoneHeaders, Origin: "https://another.ts.net:8443" }, payload: presence,
  })).status, 403);
  for (const [route, method] of [
    ["/api/games/detectors", "GET"], ["/api/games/detection", "POST"],
    ["/api/games/detection-result", "POST"], ["/api/games/library", "POST"],
    ["/api/games/review", "POST"], ["/api/games/import", "POST"],
    ["/api/repertoires", "POST"], ["/api/study/analysis-context", "POST"],
    ["/api/study", "PUT"], ["/api/study", "GET"], ["/api/study/presence", "GET"],
  ]) assert.equal((await send(host, route, { method, headers: phoneHeaders, payload: {} })).status, 404, route);
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
  const input = { dataset_id, name: "Ingress fixture", learner_color: "white", pgn: "1. e4 *" };
  const preview = await post("/api/repertoires", { ...input, command: "preview" });
  await post("/api/repertoires", { ...input, command: "save", token: preview.token });
  const issued = await post("/api/study", { command: "next" }, phoneHeaders);
  const resumed = await post("/api/study", { command: "resume", attempt_id: issued.attempt_id });
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
