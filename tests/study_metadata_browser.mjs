// Exercise the mounted Study continuation and recovery branches with identical
// issued records that differ only in the presentation name.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { DatabaseSync, backup } from "node:sqlite";
import { setTimeout as pause } from "node:timers/promises";
import { databaseRows } from "./support/game_detection_readback.mjs";

const args = process.argv.slice(2);
assert.equal(args.length, 2, "Usage: node study_metadata_browser.mjs CANDIDATE_BINARY CANDIDATE_UI");
const [binary, ui] = args.map(path => resolve(path));
const { chromium } = await import(process.env.TEMPO_PLAYWRIGHT || "playwright");
const directory = await mkdtemp(join(tmpdir(), "tempo-metadata-study-"));
let child;
let browser;
let origin;

async function start(database) {
  child = spawn(binary, ["serve", database, ui, "0"]);
  origin = await new Promise((resolve, reject) => {
    let stdout = "";
    let stderr = "";
    const timer = setTimeout(() => reject(new Error(`Tempo startup timed out. ${stderr}`)), 8000);
    child.stderr.on("data", chunk => { stderr += chunk; });
    child.stdout.on("data", chunk => {
      stdout += chunk;
      const listening = /^Tempo study at (http:\/\/127\.0\.0\.1:\d+)$/m.exec(stdout);
      if (listening) {
        clearTimeout(timer);
        resolve(listening[1]);
      }
    });
    child.once("error", error => { clearTimeout(timer); reject(error); });
    child.once("exit", code => { clearTimeout(timer); reject(new Error(`Tempo exited (${code}). ${stderr}`)); });
  });
}

async function stop() {
  if (child && child.exitCode === null && child.signalCode === null) {
    const exited = once(child, "exit");
    child.kill();
    await exited;
  }
  child = undefined;
}

async function post(route, body) {
  const reply = await fetch(origin + "/api/" + route, {
    method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body),
  });
  const value = await reply.json();
  assert.equal(reply.status, 200, JSON.stringify(value));
  return value;
}

async function clickMove(page, uci, fen, flipped = false) {
  await page.waitForFunction(expected =>
    document.querySelector('[aria-label="Study chessboard"][aria-disabled="false"]')?.dataset.fen === expected, fen);
  const board = page.locator(".study-board cg-board");
  await board.scrollIntoViewIfNeeded();
  const rect = await board.boundingBox();
  assert.ok(rect);
  for (const offset of [0, 2]) {
    const file = uci.charCodeAt(offset) - "a".charCodeAt(0);
    const rank = 8 - Number(uci[offset + 1]);
    await page.mouse.click(
      rect.x + ((flipped ? 7 - file : file) + 0.5) * rect.width / 8,
      rect.y + ((flipped ? 7 - rank : rank) + 0.5) * rect.height / 8,
    );
  }
}

function studyReply(page, command) {
  return page.waitForResponse(response => response.url().endsWith("/api/study")
    && response.request().postDataJSON().command === command).then(response => response.json());
}

function presentationInvariantRows(database) {
  return databaseRows(database)
    .filter(([name]) => ["assessments", "attempts", "daily_tactics_allowances", "learning", "receipts", "store_state"].includes(name))
    .map(([name, records]) => [name, records.map(row => Object.fromEntries(Object.entries(row).map(([key, value]) => {
      if (!["record", "result", "state"].includes(key)) return [key, value];
      // The actual local host uses the wall clock. Calendar day and each
      // transition stay exact; only observed millisecond timing may vary.
      const normalized = JSON.parse(value, (field, entry) =>
        field === "unix_ms" ? 0 : field === "repertoire_name" ? undefined : entry);
      return [key, normalized];
    })))]);
}

async function seed(kind) {
  const database = join(directory, kind + "-seed.sqlite");
  execFileSync(binary, ["init", database]);
  if (kind === "tactic") {
    const transfer = join(directory, "tactic.json");
    await writeFile(transfer, JSON.stringify({ schema_version: 1, puzzles: [{
      namespace: "tempo-display-test",
      source_id: "03ySM",
      solver_fen: "2r3k1/5pp1/Q3b2p/3p4/3q4/3B3P/1P3PP1/4R1K1 w - - 0 27",
      solution: ["a6c8", "e6c8", "e1e8"],
      opponent_lead_ins: [{
        before_fen: "2r3k1/5pp1/Q3b2p/3pq3/8/3B3P/1P3PP1/4R1K1 b - - 0 26",
        uci: "e5d4",
      }],
      label: "Mate in two",
      provenance: ["Lichess CC0 fixture"],
    }] }));
    execFileSync(binary, ["import-tactics", database, transfer]);
  }
  await start(database);
  if (kind === "teach") {
    const { dataset_id } = await post("repertoires", { command: "list" });
    const input = { dataset_id, name: "Fixture repertoire", learner_color: "black", pgn: "1. e4 c5 *" };
    const preview = await post("repertoires", { command: "preview", ...input });
    assert.equal((await post("repertoires", { command: "save", ...input, token: preview.token })).status, "repertoires");
  }
  const issued = await post("study", { command: "next" });
  assert.equal(issued.activity, kind === "tactic" ? "test" : "teach");
  await stop();
  return { database, attempt: issued.attempt_id };
}

async function clone(seed, database, name) {
  const source = new DatabaseSync(seed.database, { readOnly: true });
  try {
    await backup(source, database);
  } finally {
    source.close();
  }
  const copy = new DatabaseSync(database);
  try {
    const sql = name === undefined
      ? "UPDATE attempts SET record=json_remove(record,'$.repertoire_name') WHERE id=?"
      : "UPDATE attempts SET record=json_set(record,'$.repertoire_name',?) WHERE id=?";
    const result = copy.prepare(sql).run(...(name === undefined ? [] : [name]), seed.attempt);
    assert.equal(result.changes, 1);
  } finally {
    copy.close();
  }
}

try {
  const seeds = { tactic: await seed("tactic"), teach: await seed("teach") };
  browser = await chromium.launch({ channel: "chrome", headless: true });
  for (const journey of ["clean", "miss", "teach"]) {
    const baseline = seeds[journey === "teach" ? "teach" : "tactic"];
    let reference;
    for (const name of ["Present", "Renamed", undefined]) {
      const database = join(directory, `${journey}-${name}.sqlite`);
      await clone(baseline, database, name);
      await start(database);
      const page = await browser.newPage();
      page.setDefaultTimeout(10_000);
      await page.addInitScript(() => {
        let serial = 0;
        crypto.randomUUID = () => "00000000-0000-4000-8000-" + String(++serial).padStart(12, "0");
      });
      const commands = [];
      page.on("request", request => {
        if (request.url().endsWith("/api/study")) commands.push(request.postDataJSON());
      });
      const resumed = studyReply(page, "next");
      await page.goto(origin);
      const active = await resumed;
      assert.equal(active.exercise_name ?? undefined, name);
      assert.equal(active.attempt_id, baseline.attempt);
      assert.equal(active.opponent_move?.uci, journey === "teach" ? "e2e4" : "e5d4");

      const continued = studyReply(page, "next").catch(error => error);
      if (journey === "teach") {
        await clickMove(page, "c7c5", active.display_fen, true);
        const completion = studyReply(page, "acknowledge");
        await page.getByRole("button", { name: "Got it — continue" }).click();
        const taught = await completion;
        assert.equal(taught.status, "complete");
        assert.equal(taught.assessment, null);
      } else {
        let display = active;
        const answer = async uci => {
          const result = studyReply(page, "answer").catch(error => error);
          await clickMove(page, uci, display.display_fen);
          display = await result;
          if (display instanceof Error) throw display;
          return display;
        };
        if (journey === "miss") {
          const incorrect = await answer("a6b7");
          assert.equal(incorrect.assessment, "again");
        }
        assert.equal((await answer("a6c8")).status, "ready");
        const terminal = await answer("e1e8");
        assert.equal(terminal.status, "complete");
        if (journey === "clean") {
          assert.equal(terminal.assessment, "good");
        } else {
          assert.equal(terminal.assessment, "again");
          await pause(800);
          assert.equal(commands.filter(command => command.command === "next").length, 1);
          await page.getByRole("button", { name: "Continue", exact: true }).click();
        }
      }
      const next = await continued;
      if (next instanceof Error) throw next;
      assert.equal(next.status, "empty");
      const evidence = { commands, rows: presentationInvariantRows(database) };
      if (reference) assert.deepEqual(evidence, reference);
      else reference = evidence;
      await page.close();
      await stop();
    }
    console.log(`Mounted Study ${journey}: names present, renamed and omitted produced the same command sequence and durable effects.`);
  }
} finally {
  await browser?.close();
  await stop();
  await rm(directory, { recursive: true, force: true });
}
