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
const splitAliasSeed = join(scratch, "split-origin.sqlite");
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
  const solver_fen = "4k3/7p/2n5/8/8/8/4B3/6K1 w - - 0 1";
  const base = { namespace: "tempo1-card", source_id: "later-pin", label: "Game tactic", solver_fen, solution };
  const captured = {
    ...base, provenance: ["tempo1:capture:later-pin", "tempo1:source_kind:game"],
    legacy_game_captures: [{ capture_id: "later-pin", solver_fen, solution }],
  };
  const manual = { ...base, provenance: ["tempo1:capture:manual", "tempo1:source_kind:manual"] };
  const orphan = {
    ...base, source_id: "orphan-game-tag", provenance: ["tempo1:source_kind:game"],
    legacy_game_captures: [{ capture_id: "different-line", solver_fen, solution: ["e2d3"] }],
  };
  async function issue(database, puzzles) {
    await writeFile(transfer, JSON.stringify({ schema_version: 1, puzzles }));
    execFileSync(binary, ["init", database]);
    execFileSync(binary, ["import-tactics", database, transfer]);
    const seedUrl = await serve(database);
    const issued = await post(seedUrl, { command: "next" });
    assert.equal(issued.activity, "test");
    await stop();
    return issued.attempt_id;
  }
  const gameAttempt = await issue(seed, [captured]);
  const splitAliasAttempt = await issue(splitAliasSeed, [manual, orphan]);

  browser = await chromium.launch({ channel: "chrome", headless: true });
  const results = [];
  for (const [index, name] of ["Game tactic", "Renamed", null, "Split origin"].entries()) {
    const capturedFromGame = index !== 3;
    const db = join(scratch, `case-${index}.sqlite`);
    await copyFile(capturedFromGame ? seed : splitAliasSeed, db);
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
    await page.getByText(/Play Bb5/).waitFor();
    await clue.waitFor();
    const text = await clue.innerText();
    assert.match(text, /^Pin\. After Bb5, the bishop on b5 pins the knight on c6.*king on e8/);
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
    assert.deepEqual(beforeNext.grades, [{
      attempt_id: capturedFromGame ? gameAttempt : splitAliasAttempt, grade: "again",
    }]);
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
  assert.equal(results[3].text, results[0].text);
  console.log("Chrome verified the later pin across sources, flipped board, history, name invariance, exact Answer replay, and Retry Next.");

  for (const missed of [false, true]) {
    const db = join(scratch, `checking-fork-${missed}.sqlite`);
    const id = await issue(db, [{
      namespace: "puzzle-pack", source_id: "checking-fork", label: "Fork", provenance: [],
      solver_fen: "r2qk1nr/ppp3pp/2p1p3/2b5/4P3/8/PPPP1PPP/RNBQK2R w KQkq - 0 1",
      solution: ["d1h5", "g7g6", "h5c5"],
    }]);
    const url = await serve(db);
    const page = await browser.newPage();
    const replies = [];
    await page.route(api, async route => {
      const response = await route.fetch();
      replies.push(await response.text());
      await route.fulfill({response});
    });
    await page.goto(url);
    await page.getByText("Move 1 of 2").waitFor();
    if (missed) {
      await move(page, "d1e2");
      const clue = page.locator(clueId);
      await clue.waitFor();
      assert.match(await clue.innerText(), /Queen fork.*Qh5\+ checks the king on e8 and attacks the bishop on c5/s);
      await page.waitForFunction(label => {
        const hashes = [...document.querySelectorAll(label + " g[cgHash]")].map(g => g.getAttribute("cgHash"));
        return [",h5,e8,green", ",h5,c5,green"].every(key => hashes.some(hash => hash.includes(key)));
      }, boardLabel);
    }
    assert.ok(replies.every(raw => !raw.includes("h5c5") && !raw.includes("Qxc5")));
    await move(page, "d1h5");
    await page.getByText("Move 2 of 2").waitFor();
    await move(page, "h5c5");
    await page.waitForFunction(() => !document.body.innerText.includes("Move 2 of 2"));
    assert.deepEqual(durable(db).grades,[{attempt_id:id,grade:missed?"again":"good"}]);
    await page.close();
    await stop();
  }
  console.log("Chrome verified the fork, spoiler boundary, two playable decisions and one sticky assessment.");
  for (const example of [
    {
      id: "defended-ray", fen: "r3k2r/ppp2pp1/3p3p/4p2P/2B1P2q/3PB2P/PbP2P2/1R1Q1RK1 b kq - 0 1",
      line: ["h4h3"], wrong: "h4g5", black: true,
      hint: /^Pawn capture\. Qxh3 takes the pawn on h3\. The opponent has no immediate legal recapture\./,
    },
    {
      id: "mate-in-two", fen: "8/5Q2/k2K4/8/8/8/8/8 w - - 0 1",
      line: ["d6c5", "a6a5", "f7a2"], wrong: "f7g8", black: false,
      hint: /^Mating sequence\. Kc5 starts a sequence that ends in checkmate\./,
    },
  ]) {
    const db = join(scratch, `${example.id}.sqlite`);
    await issue(db, [{namespace: "puzzle-pack", source_id: example.id, label: "Puzzle", provenance: [],
      solver_fen: example.fen, solution: example.line}]);
    const url = await serve(db);
    const page = await browser.newPage();
    await page.goto(url);
    await move(page, example.wrong, example.black);
    const clue = page.locator(clueId);
    await clue.waitFor();
    assert.match(await clue.innerText(), example.hint);
    await page.close();
    await stop();
  }
  const freeCapture = join(scratch, "free-capture.sqlite");
  await issue(freeCapture, [{
    namespace: "puzzle-pack", source_id: "free-capture", label: "Pack tactic", provenance: [],
    solver_fen: "7k/8/8/8/3n4/8/8/3R3K w - - 0 1", solution: ["d1d4"],
  }]);
  const captureUrl = await serve(freeCapture);
  const page = await browser.newPage();
  await page.goto(captureUrl);
  const captureClue = page.locator(clueId);
  assert.equal(await captureClue.count(), 0);
  await move(page, "d1d2");
  await captureClue.waitFor();
  assert.match(await captureClue.innerText(),
    /^Hanging piece\. Rxd4 takes the knight on d4\. The opponent has no immediate legal recapture\./);
  await page.waitForFunction(label => {
    const shapes = [...document.querySelectorAll(label + " svg.cg-shapes g[cgHash]")]
      .map(shape => shape.getAttribute("cgHash"));
    return [",d1,d4,blue", ",d4,green"].every(fragment => shapes.some(shape => shape.includes(fragment)));
  }, boardLabel);
  await page.close();
  await stop();
  console.log("Chrome verified the hanging-piece label, explanatory text, arrow and ring on a pack tactic.");
} finally {
  await browser?.close();
  await stop();
  await rm(scratch, { recursive: true, force: true });
}
