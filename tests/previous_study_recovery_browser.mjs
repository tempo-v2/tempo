// Reuse exact prior installed assets on disposable databases, then reload the
// same browser tab into the candidate with the old immutable envelope intact.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { cp, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { setTimeout as delay } from "node:timers/promises";
import { join, resolve } from "node:path";
import { databaseRows } from "./support/game_detection_readback.mjs";

const args = process.argv.slice(2);
assert.equal(args.length, 4,
  "Usage: node previous_study_recovery_browser.mjs OLD_BINARY OLD_UI CANDIDATE_BINARY CANDIDATE_UI");
const [priorBinary, priorUi, binary, ui] = args.map(path => resolve(path));
const { chromium } = await import(process.env.TEMPO_PLAYWRIGHT || "playwright");
const directory = await mkdtemp(join(tmpdir(), "tempo-prior-study-"));
const previous = join(directory, "previous-tempo");
const previousUi = join(directory, "previous-ui");
const recoveryKey = "tempo-study-recovery-v4";
const studyApi = "**/api/study";
const repertoireApi = "**/api/repertoires";
const repertoireRecoveryKey = "tempo-repertoire-recovery-v4";
const fixture = {
  schema_version: 1,
  puzzles: [{
    namespace: "tempo-upgrade-test",
    source_id: "03ySM",
    solver_fen: "2r3k1/5pp1/Q3b2p/3p4/3q4/3B3P/1P3PP1/4R1K1 w - - 0 27",
    solution: ["a6c8", "e6c8", "e1e8"],
    label: "Mate in two",
    provenance: ["Lichess CC0 fixture"],
  }],
};
let child;
let browser;

async function start(executable, database, assets, port = "0") {
  child = spawn(executable, ["serve", database, assets, port]);
  return new Promise((resolve, reject) => {
    let stdout = "";
    let stderr = "";
    const deadline = setTimeout(() => reject(new Error(`Tempo startup timed out. ${stderr}`)), 8000);
    child.stderr.on("data", chunk => { stderr += chunk; });
    child.stdout.on("data", chunk => {
      stdout += chunk;
      const listening = /^Tempo study at (http:\/\/127\.0\.0\.1:\d+)$/m.exec(stdout);
      if (listening) {
        clearTimeout(deadline);
        resolve(listening[1]);
      }
    });
    child.once("error", error => { clearTimeout(deadline); reject(error); });
    child.once("exit", code => { clearTimeout(deadline); reject(new Error(`Tempo exited (${code}). ${stderr}`)); });
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

function count(snapshot, table) {
  return snapshot.find(([name]) => name === table)[1].length;
}

function assertReplayPreservedRows(after, before) {
  const oldTables = new Set(before.map(([name]) => name));
  for (const [name, rows] of after.filter(([name]) => !oldTables.has(name))) {
    assert.ok(["repertoire_suspensions", "game_participation_revisions"].includes(name));
    assert.deepEqual(rows, []);
  }
  assert.deepEqual(after.filter(([name]) => oldTables.has(name)), before);
}

async function missTactic(page) {
  const board = page.locator(".study-board cg-board");
  await board.scrollIntoViewIfNeeded();
  const rect = await board.boundingBox();
  assert.ok(rect);
  for (const [file, rank] of [[0, 2], [1, 1]]) {
    await page.mouse.click(rect.x + (file + 0.5) * rect.width / 8, rect.y + (rank + 0.5) * rect.height / 8);
  }
}

try {
  await cp(priorBinary, previous, { preserveTimestamps: true });
  await cp(priorUi, previousUi, { recursive: true });
  const transfer = join(directory, "tactic.json");
  await writeFile(transfer, JSON.stringify(fixture));
  browser = await chromium.launch({ channel: "chrome", headless: true });

  for (const kind of ["answer", "learn_more_tactics"]) {
    for (const acceptedBeforeUpgrade of [true, false]) {
      const database = join(directory, `${kind}-${acceptedBeforeUpgrade}.sqlite`);
      execFileSync(previous, ["init-practice", database]);
      execFileSync(previous, ["import-tactics", database, transfer]);
      if (kind === "learn_more_tactics") execFileSync(previous, ["configure", database, "0", "0", "1"]);

      const origin = await start(previous, database, previousUi);
      const page = await browser.newPage();
      page.setDefaultTimeout(6000);
      await page.goto(origin);
      const allowanceButton = page.getByRole("button", { name: "Learn up to 1 more tactic today" });
      if (kind === "answer") {
        await page.locator(".study-board cg-board").waitFor();
      } else {
        await allowanceButton.waitFor();
      }
      const prior = databaseRows(database);
      let priorReply;
      let immutableBytes;
      await page.route(studyApi, async route => {
        const request = route.request();
        if (request.postDataJSON().command !== kind) {
          await route.continue();
          return;
        }
        immutableBytes = request.postData();
        if (acceptedBeforeUpgrade) {
          const accepted = await route.fetch();
          assert.equal(accepted.status(), 200);
          priorReply = await accepted.json();
          assert.ok(["ready", "empty"].includes(priorReply.status), JSON.stringify(priorReply));
        }
        await route.abort("failed");
      });

      if (kind === "answer") await missTactic(page);
      else await allowanceButton.click();
      await page.getByRole("button", { name: "Retry", exact: true }).waitFor();
      assert.ok(immutableBytes);
      const oldEnvelope = await page.evaluate(key => sessionStorage.getItem(key), recoveryKey);
      const immutable = JSON.parse(immutableBytes);
      assert.deepEqual(JSON.parse(oldEnvelope).pending, immutable);
      if (kind === "answer") assert.equal(immutable.answer.uci, "a6b7");
      const afterOld = databaseRows(database);
      assert.equal(count(afterOld, "receipts"), count(prior, "receipts") + Number(acceptedBeforeUpgrade));

      await page.unroute(studyApi);
      await stop();
      assert.equal(await start(binary, database, ui, new URL(origin).port), origin);
      const commandsAfterUpgrade = [];
      if (kind === "learn_more_tactics") {
        // The accepted allowance normally continues with a separate Next.
        // Interrupt it once so readback can isolate the replay's durable effect.
        let interruptNext = true;
        await page.route(studyApi, route => {
          const command = route.request().postDataJSON().command;
          commandsAfterUpgrade.push(command);
          if (command === "next" && interruptNext) {
            interruptNext = false;
            return route.abort("failed");
          }
          return route.continue();
        });
      }
      const candidate = page.waitForResponse(response =>
        response.url().endsWith("/api/study") && response.request().postDataJSON().command === kind);
      await page.reload();
      const response = await candidate;
      assert.equal(response.status(), 200);
      assert.equal(response.request().postData(), immutableBytes, "candidate replay must send exact old bytes");
      const result = await response.json();
      assert.equal(result.status, kind === "answer" ? "ready" : "empty", JSON.stringify(result));
      if (priorReply) assert.deepEqual(result, priorReply);
      if (kind === "learn_more_tactics") {
        await page.getByRole("button", { name: "Retry next exercise" }).waitFor();
      } else {
        await page.waitForFunction(key => JSON.parse(sessionStorage.getItem(key)).pending === null, recoveryKey);
      }

      const after = databaseRows(database);
      assert.equal(count(after, "receipts"), count(prior, "receipts") + 1);
      if (kind === "answer") {
        assert.equal(result.assessment, "again");
        assert.equal(count(after, "assessments"), count(prior, "assessments") + 1);
      } else {
        assert.equal(count(after, "assessments"), count(prior, "assessments"));
        assert.equal(count(after, "daily_tactics_allowances"), count(prior, "daily_tactics_allowances") + 1);
      }
      if (acceptedBeforeUpgrade) assertReplayPreservedRows(after, afterOld);
      if (kind === "learn_more_tactics") {
        const next = page.waitForResponse(response =>
          response.url().endsWith("/api/study") && response.request().postDataJSON().command === "next");
        await page.getByRole("button", { name: "Retry next exercise" }).click();
        assert.equal((await (await next).json()).status, "ready");
        assert.deepEqual(commandsAfterUpgrade, ["learn_more_tactics", "next", "next"]);
        assert.equal(count(databaseRows(database), "receipts"), count(prior, "receipts") + 1);
      }
      await page.close();
      await stop();
      console.log(`Replayed prior installed ${kind} (${acceptedBeforeUpgrade ? "accepted, reply lost" : "unaccepted"}) exactly once.`);
    }
  }

  for (const acceptedBeforeUpgrade of [true, false]) {
    const database = join(directory, "repertoire-save-" + acceptedBeforeUpgrade + ".sqlite");
    execFileSync(previous, ["init-practice", database]);
    const origin = await start(previous, database, previousUi);
    const page = await browser.newPage();
    page.setDefaultTimeout(6000);
    await page.goto(origin);
    await page.getByRole("button", { name: "Repertoires", exact: true }).click();
    await page.getByLabel("Repertoire name").fill("Previous release PGN");
    await page.getByLabel("Learner color").selectOption("white");
    await page.getByLabel("Or paste PGN").fill("1. e4 *");
    await page.getByRole("button", { name: "Preview PGN", exact: true }).click();
    let immutableBytes;
    let priorReply;
    await page.route(repertoireApi, async route => {
      const request = route.request();
      if (request.postDataJSON().command !== "save") return route.continue();
      immutableBytes = request.postData();
      if (acceptedBeforeUpgrade) {
        const accepted = await route.fetch();
        assert.equal(accepted.status(), 200);
        priorReply = await accepted.text();
        const reply = JSON.parse(priorReply);
        assert.equal(reply.status, "repertoires");
        assert.ok(!Object.hasOwn(reply.repertoires[0], "activity"));
      }
      await route.abort("failed");
    });
    await page.getByRole("button", { name: "Create repertoire", exact: true }).click();
    await page.getByRole("button", { name: "Retry save", exact: true }).waitFor();
    const pending = await page.evaluate(key => sessionStorage.getItem(key), repertoireRecoveryKey);
    assert.equal(pending, immutableBytes);
    if (acceptedBeforeUpgrade) {
      // Let the independent cache projection settle before comparing complete database readbacks.
      const deadline = Date.now() + 6000;
      while (count(databaseRows(database), "game_model_projection") === 0) {
        assert.ok(Date.now() < deadline, "The prior release did not finish projecting the saved book.");
        await delay(50);
      }
    }
    const afterOld = databaseRows(database);
    assert.equal(count(afterOld, "content_receipts"), Number(acceptedBeforeUpgrade));

    await page.unroute(repertoireApi);
    await stop();
    assert.equal(await start(binary, database, ui, new URL(origin).port), origin);
    const candidate = page.waitForResponse(response =>
      response.url().endsWith("/api/repertoires") && response.request().postDataJSON().command === "save");
    await page.reload();
    const response = await candidate;
    assert.equal(response.status(), 200);
    assert.equal(response.request().postData(), immutableBytes);
    const reply = await response.text();
    if (priorReply) assert.equal(reply, priorReply);
    const result = JSON.parse(reply);
    assert.equal(result.status, "repertoires");
    if (!acceptedBeforeUpgrade) assert.equal(result.repertoires[0].activity, "active");
    await page.getByRole("button", {name: "Edit Previous release PGN", exact: true}).waitFor();
    assert.equal(await page.evaluate(key => sessionStorage.getItem(key), repertoireRecoveryKey), null);

    const after = databaseRows(database);
    assert.equal(count(after, "content_receipts"), 1);
    assert.equal(count(after, "repertoire_documents"), 1);
    if (acceptedBeforeUpgrade) assertReplayPreservedRows(after, afterOld);
    await page.close();
    await stop();
    console.log(`Replayed prior installed repertoire save (${acceptedBeforeUpgrade ? "accepted, reply lost" : "unaccepted"}) exactly once.`);
  }
} finally {
  await browser?.close();
  await stop();
  await rm(directory, { recursive: true, force: true });
}
