// Run against a local candidate with TEMPO_BINARY set. Uses isolated data and loopback.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, mkdir, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { request } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { DatabaseSync } from "node:sqlite";
import { randomUUID } from "node:crypto";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";

const binary = resolve(process.env.TEMPO_BINARY || "target/debug/tempo");

async function fixture(t) {
  const directory = await mkdtemp(join(tmpdir(), "tempo-transport-"));
  const database = join(directory, "study.sqlite");
  const assets = join(directory, "ui");
  const logs = join(directory, "diagnostics");
  await mkdir(join(assets, "assets"), { recursive: true });
  await writeFile(join(assets, "index.html"), "Transport fixture");
  await writeFile(join(assets, "assets", "fixture.js"), "console.log(42);");
  execFileSync(binary, ["init", database]);

  const env = { ...process.env, TEMPO_LOG_DIR: logs };
  delete env.TEMPO_PHONE_ORIGIN;
  delete env.TEMPO_PHONE_TAILSCALE_LOGIN;
  const child = spawn(binary, ["serve", database, assets, "0"], { env });
  const lines = createInterface({ input: child.stdout });
  let errors = "";
  child.stderr.on("data", data => { errors += data; });
  t.after(async () => {
    lines.close();
    if (child.exitCode === null && child.signalCode === null) {
      const stopped = once(child, "exit");
      child.kill();
      await stopped;
    }
    await rm(directory, { recursive: true, force: true });
  });

  for await (const line of lines) {
    const match = /^Tempo study at (http:\/\/127\.0\.0\.1:\d+)$/.exec(line);
    if (match) return { origin: match[1], logs, database };
  }
  assert.fail(errors || "Tempo did not start");
}

async function waitForDiagnostic(logs, delivery, matches) {
  const end = performance.now() + 2000;
  while (performance.now() < end) {
    const files = await readdir(logs).catch(() => []);
    for (const name of files.filter(name => name.endsWith(".jsonl"))) {
      const lines = (await readFile(join(logs, name), "utf8")).split("\n").slice(0, -1);
      for (const line of lines) {
        if (!line.includes(delivery)) continue;
        const event = JSON.parse(line);
        if (matches(event)) return;
      }
    }
    await delay(10);
  }
  assert.fail("No matching transport diagnostic for " + delivery);
}

function beginUpload(t, origin, payload, delivery) {
  let upload;
  const received = new Promise((resolveResponse, reject) => {
    upload = request(origin + "/api/study", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "Content-Length": Buffer.byteLength(payload),
        "X-Tempo-Request-Id": delivery,
      },
    }, response => {
      let body = "";
      response.setEncoding("utf8");
      response.on("data", data => { body += data; });
      response.on("error", reject);
      response.on("end", () => resolveResponse({ status: response.statusCode, body, headers: response.headers }));
    });
    upload.on("error", reject);
  });
  t.after(() => upload.destroy());
  // The fixture sends a normal valid command across two delivery turns.
  upload.write(payload.slice(0, 1));
  return { upload, observed: received.then(reply => ({ reply }), error => ({ error })) };
}

test("an in-progress upload leaves context available before the body completes", { timeout: 12000 }, async t => {
  const { origin, logs } = await fixture(t);
  const delivery = randomUUID();
  const payload = JSON.stringify({ command: "study_status" });
  const { upload, observed } = beginUpload(t, origin, payload, delivery);
  await waitForDiagnostic(logs, delivery, event => event.event === "http.body_started");

  const response = await fetch(origin + "/api/context", { signal: AbortSignal.timeout(2000) });
  assert.equal(response.status, 200);
  const context = await response.json();
  assert.equal(context.status, "ready");

  upload.end(payload.slice(1));
  const { reply, error } = await observed;
  if (error) throw error;
  assert.equal(reply.status, 200, reply.body);
  assert.equal(reply.headers["x-tempo-request-id"], delivery);
  assert.equal(JSON.parse(reply.body).dataset_id, context.dataset_id);
});

test("an incomplete upload expires before application admission", { timeout: 17000 }, async t => {
  const { origin, logs } = await fixture(t);
  const delivery = randomUUID();
  const payload = JSON.stringify({ command: "study_status" });
  const { observed } = beginUpload(t, origin, payload, delivery);
  await waitForDiagnostic(logs, delivery, event => event.event === "http.body_started");

  const { reply, error } = await observed;
  if (error) throw error;
  assert.equal(reply.status, 408, reply.body);
  assert.equal(JSON.parse(reply.body).code, "request_expired");
  const response = await fetch(origin + "/api/context", { signal: AbortSignal.timeout(2000) });
  assert.equal(response.status, 200);
  for (const name of (await readdir(logs)).filter(name => name.endsWith(".jsonl"))) {
    const file = await readFile(join(logs, name), "utf8");
    const events = file.split("\n").slice(0, -1).filter(line => line.includes(delivery)).map(JSON.parse);
    assert.equal(events.some(event => event.operation === "http.request"), false);
  }
});

test("authorized context and assets finish while the foreground owner waits on SQLite", { timeout: 15000 }, async t => {
  const { origin, logs, database } = await fixture(t);
  const post = (body, extraHeaders = {}) => fetch(origin + "/api/tactic-settings", {
    method: "POST",
    headers: { "Content-Type": "application/json", ...extraHeaders },
    body: JSON.stringify(body),
  });
  const prior = await (await post({ command: "get" })).json();
  const delivery = randomUUID();
  const sql = new DatabaseSync(database);
  let saving;
  try {
    sql.exec("BEGIN IMMEDIATE");
    saving = post({
      command: "save", dataset_id: prior.dataset_id, expected_revision: prior.revision,
      new_tactics_per_day: prior.new_tactics_per_day + 1, selection: prior.selection,
    }, { "X-Tempo-Request-Id": delivery }).then(response => {
      assert.equal(response.status, 200);
      return response.json();
    });
    // Keep the mutation rejection observed even if an independent read fails.
    saving.catch(() => {});
    await waitForDiagnostic(logs, delivery, event =>
      event.event === "span.start" && event.operation === "http.request");

    const get = (path, headers) => fetch(origin + path, {
      headers, signal: AbortSignal.timeout(1500),
    });
    const [context, page, asset, missing, forbidden] = await Promise.all([
      get("/api/context"), get("/"), get("/assets/fixture.js?revision=1"),
      get("/assets/missing.js"), get("/api/context", { Origin: "https://different.invalid" }),
    ]);
    assert.equal(context.status, 200);
    assert.equal(context.headers.get("cache-control"), "no-store");
    const identity = await context.json();
    assert.equal(identity.status, "ready");
    assert.equal(identity.dataset_id, prior.dataset_id);
    assert.ok(Number.isInteger(identity.schema_version));
    assert.equal(await page.text(), "Transport fixture");
    assert.match(page.headers.get("content-security-policy"), /frame-ancestors 'none'/);
    assert.equal(await asset.text(), "console.log(42);");
    assert.equal(asset.headers.get("x-content-type-options"), "nosniff");
    assert.equal(missing.status, 404);
    assert.equal(forbidden.status, 403);

    const priorBytes = sql.prepare("SELECT local_profile FROM store_state").get().local_profile;
    assert.equal(JSON.parse(priorBytes).introductions.tactics, prior.new_tactics_per_day);
    sql.exec("COMMIT");
  } finally {
    sql.close();
  }
  const saved = await saving;
  assert.equal(saved.new_tactics_per_day, prior.new_tactics_per_day + 1);
});
