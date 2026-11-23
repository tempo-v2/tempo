// Run against a local candidate with TEMPO_BINARY set. Uses isolated data and loopback.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, mkdir, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { request } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
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
    if (match) return { origin: match[1], logs };
  }
  assert.fail(errors || "Tempo did not start");
}

async function waitingForBody(logs, delivery) {
  const end = performance.now() + 2000;
  while (performance.now() < end) {
    const files = await readdir(logs).catch(() => []);
    for (const name of files.filter(name => name.endsWith(".jsonl"))) {
      const lines = (await readFile(join(logs, name), "utf8")).split("\n").slice(0, -1);
      for (const line of lines) {
        if (!line.includes(delivery)) continue;
        const event = JSON.parse(line);
        if (event.event === "http.body_started") return;
      }
    }
    await delay(10);
  }
  assert.fail("No diagnostic for the admitted request body");
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
  await waitingForBody(logs, delivery);

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
  await waitingForBody(logs, delivery);

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
