// Run from the assembled workspace with the built Tempo binary and desktop UI.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { copyFile, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { DatabaseSync } from "node:sqlite";

const { chromium } = await import(process.env.TEMPO_PLAYWRIGHT || "playwright");
const binary = resolve(process.env.TEMPO_BINARY || "target/release/tempo");
const ui = resolve("tempo-ui/dist");
const scratch = await mkdtemp(join(tmpdir(), "tempo-study-explanation-"));
const api = "**/api/study";
const boardLabel = '[aria-label="Study chessboard"]';
const clueId = "#study-tactic-clue";
const seed = join(scratch, "baseline.sqlite");
const solution = ["g1h1", "h7h6", "e2b5"];
let server, browser;

async function serve(db) {
  server = spawn(binary, ["serve", db, ui, "0"], { stdio: ["ignore", "pipe", "inherit"] });
  const output = createInterface({ input: server.stdout });
  return Promise.race([
    once(output, "line").then(([line]) => line.replace("Tempo study at ", "")),
    once(server, "exit").then(([code]) => { throw new Error(`Tempo exited (${code})`); }),
  ]);
}

async function stop() {
  if (!server || server.exitCode !== null) return;
  const exited = once(server, "exit");
  server.kill();
  await exited;
  server = null;
}

async function post(url, command) {
  const reply = await fetch(url + "/api/study", {
    headers: { "Content-Type": "application/json" }, method: "POST", body: JSON.stringify(command),
  });
  assert.equal(reply.status, 200);
  return reply.json();
}

async function move(page, uci, flipped = false) {
  const board = page.locator(boardLabel).locator("cg-board");
  await page.waitForFunction(label => document.querySelector(label)?.getAttribute("aria-disabled") === "false", boardLabel);
  await board.scrollIntoViewIfNeeded();
  const rect = await board.boundingBox();
  for (const offset of [0, 2]) {
    const file = uci.charCodeAt(offset) - 97;
    const rank = 8 - Number(uci[offset + 1]);
    await page.mouse.click(
      rect.x + ((flipped ? 7 - file : file) + 0.5) * rect.width / 8,
      rect.y + ((flipped ? 7 - rank : rank) + 0.5) * rect.height / 8,
    );
  }
}

function durable(db) {
  const sql = new DatabaseSync(db, { readOnly: true });
  try {
    return {
      receipts: sql.prepare("SELECT id, command, result FROM receipts ORDER BY id").all(),
      grades: sql.prepare("SELECT attempt_id, json_extract(record, '$.input.grade') AS grade FROM assessments")
        .all().map(({ attempt_id, grade }) => ({ attempt_id, grade })),
      schedule: sql.prepare(`SELECT target_id, json_extract(state, '$.due') AS due,
        json_extract(state, '$.miss_revision') AS miss_revision FROM learning ORDER BY target_id`).all(),
    };
  } finally {
    sql.close();
  }
}

try {
  const transfer = join(scratch, "tactics.json");
  await writeFile(transfer, JSON.stringify({
    schema_version: 1,
    puzzles: [{
      namespace: "study-feedback-fixture", source_id: "later-pin", label: "Game tactic",
      solver_fen: "4k3/7p/2n5/8/8/8/4B3/6K1 w - - 0 1", solution,
      provenance: ["tempo1:capture:later-pin", "tempo1:source_kind:game"],
    }],
  }));
  execFileSync(binary, ["init", seed]);
  execFileSync(binary, ["import-tactics", seed, transfer]);
  const seedUrl = await serve(seed);
  const issued = await post(seedUrl, { command: "next" });
  assert.equal(issued.activity, "test");
  await stop();

  browser = await chromium.launch({ channel: "chrome", headless: true });
  const results = [];
  for (const [index, name] of ["Game tactic", "Renamed", null].entries()) {
    const db = join(scratch, `case-${index}.sqlite`);
    await copyFile(seed, db);
    const url = await serve(db);
    const page = await browser.newPage({ viewport: { width: 1250, height: 850 } });
    page.setDefaultTimeout(8000);
    await page.addInitScript(() => {
      let sequence = 0;
      crypto.randomUUID = () => `00000000-0000-4000-8000-${String(++sequence).padStart(12, "0")}`;
    });
    const commands = [];
    let loseMiss = true;
    let failNext = false;
    await page.route(api, async route => {
      const raw = route.request().postData();
      const command = JSON.parse(raw);
      if (command.command !== "study_status") commands.push(raw);
      if (failNext && command.command === "next") {
        failNext = false;
        return route.abort("failed");
      }
      const response = await route.fetch();
      const view = await response.json();
      if (loseMiss && command.answer?.uci === "e2d3") {
        loseMiss = false;
        return route.abort("failed");
      }
      if (view.attempt_id) {
        if (name) view.exercise_name = name;
        else delete view.exercise_name;
      }
      await route.fulfill({ response, json: view });
    });

    const button = text => page.getByRole("button", { name: text, exact: true });
    const clue = page.locator(clueId);
    await page.goto(url);
    await page.getByText("Move 1 of 2").waitFor();
    assert.equal(await clue.count(), 0);
    await move(page, solution[0]);
    await page.getByText("Move 2 of 2").waitFor();
    assert.equal(await clue.count(), 0);
    await button("Flip board").click();
    await page.locator(".study-board .orientation-black").waitFor();
    await move(page, "e2d3", true);
    await button("Retry").click();
    await clue.waitFor();
    const text = await clue.innerText();
    assert.match(text, /After Bb5, the bishop on b5 pins the knight on c6.*king on e8/);
    const fragments = [",e2,b5,blue", ",b5,e8,green", ",c6,green", ",e8,green"];
    await page.waitForFunction(([label, fragments]) => {
      const shapes = [...document.querySelectorAll(label + " svg.cg-shapes g[cgHash]")]
        .map(shape => shape.getAttribute("cgHash"));
      return fragments.every(fragment => shapes.some(shape => shape.includes(fragment)));
    }, [boardLabel, fragments]);
    assert.equal(commands[2], commands[3], "Retry must replay the original Answer");
    const oldReply = await post(url, JSON.parse(commands[2]));
    assert.equal(oldReply.feedback.correction_uci, solution[2]);
    assert.equal(oldReply.feedback.tactic_explanation, undefined);
    await button("Beginning").click();
    assert.equal(await clue.count(), 0);
    assert.equal(await page.locator(boardLabel + ' g[cgHash*=",green"]').count(), 0);
    await button("Return to current position").click();
    await clue.waitFor();
    await move(page, solution[2], true);
    await button("Continue").waitFor();
    assert.equal(await clue.count(), 0);
    await page.getByText("Completed after correction. Review recorded as Again.").waitFor();
    const beforeNext = durable(db);
    assert.deepEqual(beforeNext.grades, [{ attempt_id: issued.attempt_id, grade: "again" }]);
    assert.equal(beforeNext.receipts.length, 3);
    failNext = true;
    await button("Continue").click();
    await button("Retry next exercise").click();
    await page.getByText("Done for today.").waitFor();
    assert.deepEqual(durable(db), beforeNext);
    assert.deepEqual(commands.map(raw => JSON.parse(raw).command),
      ["next", "answer", "answer", "answer", "answer", "next", "next"]);
    results.push({ text, commands, ...beforeNext });
    await page.close();
    await stop();
  }
  assert.deepEqual(results[1], results[0]);
  assert.deepEqual(results[2], results[0]);
  console.log("Chrome verified the later pin, flipped board, history, name invariance, exact Answer replay, and Retry Next.");
} finally {
  await browser?.close();
  await stop();
  await rm(scratch, { recursive: true, force: true });
}
