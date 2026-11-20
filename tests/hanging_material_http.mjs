// Build the normal Tempo binary, then run with TEMPO_TACTICAL_PYTHON and
// TEMPO_STOCKFISH pointing to chess==1.11.2 and a real Stockfish executable.
// TEMPO_BROWSER=1 additionally exercises the built Games/Study UI in Chromium.
// TEMPO_PLAYWRIGHT and TEMPO_CHROMIUM may select locally installed dependencies.
import assert from "node:assert/strict";
import { once } from "node:events";
import { execFileSync, spawn } from "node:child_process";
import { access, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { createStudyController } from "../../tempo-ui/study_controller.js";
import { validateDetection, validateRegistry } from "../../tempo-ui/game_detection.js";
import { validateTacticalHistory } from "../../tempo-ui/tactical_history.js";
import { databaseRows } from "./support/game_detection_readback.mjs";

const binary = process.env.TEMPO_BINARY ?? fileURLToPath(new URL("../../target/debug/tempo", import.meta.url));
const builtUi = process.env.TEMPO_UI_DIST ?? fileURLToPath(new URL("../../tempo-ui/dist", import.meta.url));
const withBrowser = process.env.TEMPO_BROWSER === "1";
const realEngine = process.env.TEMPO_TACTICAL_PYTHON && process.env.TEMPO_STOCKFISH;
const captureFen = "r5k1/pp3ppp/8/4q3/3P4/8/PP3PPP/R2Q2K1 w - - 0 1";
const stalemateFen = "7k/n7/5KQ1/8/8/8/8/R7 w - - 0 1";
const equivalentFen = "7k/5Qn1/5K2/8/8/3B4/8/8 w - - 0 1";
const blackFen = "r2q2k1/pp3ppp/8/3p4/4Q3/8/PP3PPP/R5K1 b - - 0 1";

// Capturing e5 wins a queen. Capturing a7 in the second fixture stalemates,
// while Qg7 mates. In the third fixture Qxg7# and the quiet Qf8# both mate.
// These are deliberately different from a bare rook/knight ending where many
// moves may be winning mates and would not establish a genuine missed chance.
function pgn(event, fen, moves, black = false) {
  return `[Event "${event}"]\n[White "${black ? "Opponent" : "Learner"}"]\n`
    + `[Black "${black ? "Learner" : "Opponent"}"]\n[Date "2026.11.20"]\n`
    + `[SetUp "1"]\n[FEN "${fen}"]\n\n${moves}\n`;
}

const games = [
  pgn("Missed queen", captureFen, "1. h3 0-1"),
  pgn("Captured queen", captureFen, "1. dxe5 1-0"),
  pgn("Took stalemate", stalemateFen, "1. Rxa7 1/2-1/2"),
  pgn("Preferred mate", stalemateFen, "1. Qg7# 1-0"),
  pgn("Equivalent quiet mate", equivalentFen, "1. Qf8# 1-0"),
].join("\n");

const seed = {
  namespace: "tempo-hanging-http",
  source_id: "active-before-import",
  solver_fen: "2r3k1/5pp1/Q3b2p/3p4/3q4/3B3P/1P3PP1/4R1K1 w - - 0 1",
  solution: ["a6c8", "e6c8", "e1e8"],
  label: "Existing active tactic",
  provenance: ["Disposable HTTP acceptance fixture"],
};

async function start(database, ui) {
  const child = spawn(binary, ["serve", database, ui, "0"], { env: process.env });
  const lines = createInterface({ input: child.stdout });
  let diagnostics = "";
  child.stderr.on("data", chunk => { diagnostics = (diagnostics + chunk).slice(-12000); });
  let timeout;
  try {
    const line = await Promise.race([
      once(lines, "line").then(([value]) => value),
      once(child, "exit").then(([code]) => { throw Error(`Tempo exited ${code}: ${diagnostics}`); }),
      new Promise((_, reject) => { timeout = setTimeout(() => reject(Error(`Tempo startup timed out: ${diagnostics}`)), 20000); }),
    ]);
    const match = /^Tempo study at (http:\/\/127\.0\.0\.1:\d+)$/.exec(line);
    assert.ok(match, line);
    return {
      url: match[1],
      diagnostics: () => diagnostics,
      async stop() {
        if (child.exitCode !== null || child.signalCode !== null) return;
        const ended = once(child, "exit");
        child.kill();
        const kill = setTimeout(() => child.kill("SIGKILL"), 3000);
        try { await ended; } finally { clearTimeout(kill); }
      },
    };
  } catch (error) {
    child.kill("SIGKILL");
    throw error;
  } finally {
    clearTimeout(timeout);
  }
}

function durableTraining(database) {
  const tracked = new Set(["assessments", "attempts", "learning", "receipts", "tactical_analyses", "tactical_observations", "tactical_requests"]);
  return databaseRows(database).filter(([table]) => tracked.has(table));
}

function blind(view) {
  assert.equal(view.status, "ready", JSON.stringify(view));
  const forbidden = new Set(["expected", "solution", "correct_uci", "correction_uci", "accepted_moves", "tactical"]);
  function visit(value) {
    if (!value || typeof value !== "object") return;
    for (const [key, nested] of Object.entries(value)) {
      assert.ok(!forbidden.has(key), `Blind Study reply leaked ${key}`);
      visit(nested);
    }
  }
  visit(view);
  assert.equal(view.feedback, null);
  assert.ok(Array.isArray(view.legal_moves) && view.legal_moves.length > 0);
}

test("normal runtime analyzes real games, records comparisons, and makes each missed capture next without replacing active Study",
  { skip: !realEngine && "set TEMPO_TACTICAL_PYTHON and TEMPO_STOCKFISH", timeout: 180000 }, async t => {
    const directory = await mkdtemp(join(tmpdir(), "tempo-hanging-http-"));
    const database = join(directory, "study.sqlite");
    let host, browser;
    t.after(async () => {
      await browser?.close();
      await host?.stop();
      await rm(directory, { recursive: true, force: true });
    });
    let ui = builtUi;
    try { await access(join(ui, "index.html")); }
    catch {
      assert.equal(withBrowser, false, "TEMPO_BROWSER requires built Tempo UI artifacts");
      ui = join(directory, "ui");
      await mkdir(join(ui, "assets"), { recursive: true });
      await writeFile(join(ui, "index.html"), "<!doctype html><title>HTTP contract fixture</title>");
    }
    const transfer = join(directory, "seed.json");
    await writeFile(transfer, JSON.stringify({ schema_version: 1, puzzles: [seed] }));
    execFileSync(binary, ["init", database]);
    execFileSync(binary, ["import-tactics", database, transfer]);
    execFileSync(binary, ["configure", database, "0", "1"]);
    host = await start(database, ui);

    async function api(route, body) {
      const response = await fetch(host.url + "/api/" + route, body === undefined ? {} : {
        method: "POST", headers: { "Content-Type": "application/json", "X-Tempo-Tactics-Report":"2" }, body: JSON.stringify(body),
      });
      const value = await response.json();
      assert.ok(response.status === 200 || response.status === 202, JSON.stringify(value));
      return value;
    }
    const discovery = validateRegistry(await api("games/detectors"));
    assert.equal(discovery.status, "available");
    const detector = discovery.detectors.find(item => item.id === "personal-tactics");
    assert.ok(detector, JSON.stringify(discovery));

    let serial = 0, stored = null, loseAnswer = false, interruptNext = false, lostReply;
    const sent = [];
    function controller() {
      return createStudyController({
        transport: { async send(command) {
          sent.push(structuredClone(command));
          if (interruptNext && command.command === "next") {
            interruptNext = false;
            throw Error("Next interrupted before delivery");
          }
          const reply = await api("study", command);
          if (loseAnswer && command.command === "answer") {
            loseAnswer = false;
            lostReply = reply;
            throw Error("Saved answer reply deliberately lost");
          }
          return reply;
        } },
        storage: { load: () => stored && JSON.parse(stored), save: envelope => { stored = JSON.stringify(envelope); } },
        render() {},
        makeSubmissionId: () => `hanging-http-${++serial}`,
      });
    }
    let client = controller();
    const active = await client.start();
    blind(active);
    assert.equal(active.display_fen, seed.solver_fen);
    const dataset = active.dataset_id;
    let scanResponsiveness = null;
    const report = async () => validateTacticalHistory(await api("games/tactics", { dataset_id: dataset }), dataset);
    async function completed(total, pending) {
      const deadline = Date.now() + 100000;
      let value;
      do {
        value = await report();
        assert.notEqual(value.status, "unavailable", JSON.stringify(value));
        assert.equal(value.statistics.failed_games + value.statistics.timed_out_games + value.statistics.unsupported_games, 0, JSON.stringify(value));
        if (scanResponsiveness === null && value.status === "running") {
          const began = performance.now();
          const progress = await api("study-progress", null);
          scanResponsiveness = performance.now() - began;
          assert.ok(progress && !progress.code, JSON.stringify(progress));
          assert.ok(scanResponsiveness < 1000, `Study blocked behind engine analysis for ${scanResponsiveness}ms`);
        }
        if (value.statistics.completed_games === total && value.statistics.pending_games === 0
          && value.statistics.pending_requests === pending && value.status === "ready") return value;
        await new Promise(resolve => setTimeout(resolve, 150));
      } while (Date.now() < deadline);
      assert.fail(`Tactical analysis did not settle: ${JSON.stringify(value)}\n${host.diagnostics()}`);
    }

    const imported = await api("games/import", { dataset_id: dataset, command_id: "import-five", learner: "Learner", pgn: games });
    assert.equal(imported.new_games, 5, JSON.stringify(imported));
    const history = await completed(5, 1);
    assert.equal(history.statistics.decisions_scanned, 5);
    assert.notEqual(scanResponsiveness, null, "must measure Study while analysis is running");
    const personal = await api("games/tactics/report", {dataset_id:dataset});
    assert.equal(personal.schema_version, 2);
    assert.equal(personal.games.length, 5);
    const missed = personal.games.find(row => row.outcome === "missed_tactic"
      && row.game.result === "loss" && row.motifs.includes("hanging_material"));
    assert.ok(missed);
    assert.equal(missed.ply, 0);
    const position = async id => (await api("games/tactics/position", {dataset_id:dataset,id})).detail;
    const missedDetail = await position(missed.id);
    assert.equal(missedDetail.details.assessment.personal.puzzle.line[0].uci, "d4e5");
    assert.equal(missedDetail.training.status, "queued");
    assert.ok(personal.games.some(row => row.motifs.includes("mate_in_one")));
    assert.ok(personal.games.some(row => row.motifs.includes("mate_in_two")));
    const resumed = await api("study", { command: "next" });
    for (const key of ["attempt_id", "target_id", "revision", "display_fen"]) assert.deepEqual(resumed[key], active[key], key);
    blind(resumed);

    // Read the actual built UI using its real typed history transport.
    if (withBrowser) {
      const { chromium } = await import(process.env.TEMPO_PLAYWRIGHT ?? "playwright");
      browser = await chromium.launch({ headless: true,
        ...(process.env.TEMPO_CHROMIUM ? { executablePath: process.env.TEMPO_CHROMIUM } : {}) });
      const page = await browser.newPage({ viewport: { width: 1440, height: 1080 } });
      await page.goto(host.url + "#games");
      await page.getByText("Game analysis", { exact: true }).click();
      const panel = page.getByRole("region", { name: "Tactical history" });
      await panel.getByText("5 games checked", { exact: false }).waitFor();
      await panel.getByRole("button").filter({ hasText: "Hanging piece" }).filter({ hasText: "loss" }).click();
      const board = panel.getByRole("group", { name: "Tactical opportunity position" });
      assert.equal(await board.getAttribute("data-fen"), captureFen);
      assert.match(await panel.innerText(), /You played h3/);
      assert.match(await panel.innerText(), /queen e5/);
      await panel.getByRole("button", {name:"dxe5", exact:true}).first().click();
      assert.notEqual(await board.getAttribute("data-fen"), captureFen);
      await panel.getByRole("button", {name:"Reset board", exact:true}).click();
      assert.equal(await board.getAttribute("data-fen"), captureFen);
      if (process.env.TEMPO_HTTP_SCREENSHOTS) {
        const image = join(process.env.TEMPO_HTTP_SCREENSHOTS, "hanging_history.png");
        await mkdir(dirname(image), { recursive: true });
        await panel.screenshot({ path: image });
      }
      await panel.getByRole("button", { name: "Open source game" }).click();
      const sourceBoard = page.getByRole("group", { name: "Game review board" });
      await sourceBoard.waitFor();
      assert.equal(await sourceBoard.getAttribute("data-fen"), captureFen);
      const address = new URLSearchParams(new URL(page.url()).hash.slice(7));
      assert.equal(address.get("game_id"), missed.game.id);
      assert.equal(address.get("ply"), "0");
      await page.close();
    }

    // The older display-only detector API still validates against its strict
    // consumer. Previewing the same game does not enqueue it a second time.
    const beforeRead = durableTraining(database);
    const intent = { schema_version: 1, request_id: "legacy-preview", dataset_id: dataset,
      source: missed.game.source, game_id: missed.game.id, learner: missed.game.learner, detector_id: detector.id };
    let preview = validateDetection(await api("games/detection", intent), intent, detector);
    const previewDeadline = Date.now() + 30000;
    while (preview.status === "running" && Date.now() < previewDeadline) {
      await new Promise(resolve => setTimeout(resolve, 100));
      preview = validateDetection(await api("games/detection-result", {
        schema_version: 1, request_id: intent.request_id, dataset_id: dataset,
      }), intent, detector);
    }
    assert.equal(preview.status, "complete", JSON.stringify(preview));
    assert.equal(preview.findings.length, 1);
    assert.ok(preview.findings.every(finding => !Object.hasOwn(finding, "tactical")));
    await report();
    assert.deepEqual(durableTraining(database), beforeRead);
    const duplicated = await api("games/import", { dataset_id: dataset, command_id: "import-again", learner: "Learner", pgn: games });
    assert.equal(duplicated.new_games, 0);
    await completed(5, 1);

    // Finish the already issued card, lose the committed final reply, restart,
    // and reconstruct the actual Study controller from its pending envelope.
    assert.equal((await client.answer("a6c8")).status, "ready");
    loseAnswer = true;
    await assert.rejects(client.answer("e1e8"), /deliberately lost/);
    assert.equal(lostReply.status, "complete");
    const savedCommand = structuredClone(sent.at(-1));
    const accepted = durableTraining(database);
    await host.stop();
    host = await start(database, ui);
    client = controller();
    const recovered = await client.start();
    assert.deepEqual(sent.at(-1), savedCommand);
    assert.equal(recovered.status, "complete");
    assert.equal(recovered.attempt_id, active.attempt_id);
    assert.equal(recovered.revision, lostReply.revision);
    assert.deepEqual(durableTraining(database), accepted);
    assert.equal(JSON.parse(stored).pending, null);

    interruptNext = true;
    const nextStart = sent.length;
    await assert.rejects(client.next(), /Next interrupted/);
    assert.equal(client.getState().view.status, "complete");
    const urgent = await client.retry();
    assert.deepEqual(sent.slice(nextStart), [{ command: "next" }, { command: "next" }]);
    blind(urgent);
    assert.equal(urgent.display_fen, captureFen);
    assert.notEqual(urgent.attempt_id, active.attempt_id);

    if (withBrowser) {
      const page = await browser.newPage({ viewport: { width: 1440, height: 1080 } });
      await page.goto(host.url + "#study");
      const board = page.getByRole("group", { name: "Study chessboard" });
      await board.waitFor();
      assert.equal(await board.getAttribute("data-fen"), captureFen);
      if (process.env.TEMPO_HTTP_SCREENSHOTS) await page.screenshot({ path: join(process.env.TEMPO_HTTP_SCREENSHOTS, "urgent_study.png"), fullPage: true });
      const reply = page.waitForResponse(response => response.url().endsWith("/api/study")
        && response.request().method() === "POST" && response.request().postDataJSON().command === "answer");
      const square = await board.locator("cg-board").boundingBox();
      assert.ok(square);
      await page.mouse.click(square.x + square.width * 3.5 / 8, square.y + square.height * 4.5 / 8);
      await page.mouse.click(square.x + square.width * 4.5 / 8, square.y + square.height * 3.5 / 8);
      const solved = await (await reply).json();
      assert.equal(solved.status, "complete", JSON.stringify(solved));
      assert.equal(solved.assessment, "good");
      await page.close();
    } else assert.equal((await client.answer("d4e5")).status, "complete");
    await completed(5, 0);
    assert.equal((await position(missed.id)).training.status, "reviewed");
    assert.equal((await client.next()).status, "empty");

    // A subsequent game while idle must become the first card, even though the
    // one-card ordinary daily introduction allowance has already been used.
    const idleGame = await api("games/import", { dataset_id: dataset, command_id: "black-while-idle", learner: "Learner",
      pgn: pgn("Black missed queen", blackFen, "1... h6 1-0", true) });
    assert.equal(idleGame.new_games, 1);
    await completed(6, 1);
    const idleHistory = await api("games/tactics/report", {dataset_id:dataset});
    const blackMiss = idleHistory.games.find(row => row.outcome === "missed_tactic" && row.game.result === "loss" && row.id !== missed.id);
    assert.equal((await position(blackMiss.id)).training.status, "queued");
    const first = await client.next();
    blind(first);
    assert.equal(first.learner_color, "black");
    assert.equal(first.display_fen, blackFen);
    t.diagnostic(`Study progress responded in ${scanResponsiveness.toFixed(1)}ms during a real Stockfish scan.`);
    t.diagnostic(`Verified normal serve, six real-engine games, active-card preservation, durable recovery, next-up and idle-first priority${withBrowser ? ", Games/source boards and actual Study board capture in Chromium" : ""}.`);
  });
