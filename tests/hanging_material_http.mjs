// Build the normal Tempo binary, then run with TEMPO_TACTICAL_PYTHON and
// TEMPO_STOCKFISH pointing to chess==1.11.2 and a real Stockfish executable.
// TEMPO_BROWSER=1 additionally exercises the built Games/Study UI in Chromium.
// TEMPO_PLAYWRIGHT and TEMPO_CHROMIUM may select locally installed dependencies.
import assert from "node:assert/strict";
import { once } from "node:events";
import { execFileSync, spawn } from "node:child_process";
import { access, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { createStudyController } from "../../tempo-ui/study_controller.js";
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
    host = await start(database, ui);

    async function api(route, body) {
      const response = await fetch(host.url + "/api/" + route, body === undefined ? {} : {
        method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body),
      });
      const value = await response.json();
      assert.ok(response.status === 200 || response.status === 202, JSON.stringify(value));
      return value;
    }
    const settings = await api("tactic-settings", { command: "get" });
    const saved = await api("tactic-settings", {
      command: "save", dataset_id: settings.dataset_id, expected_revision: settings.revision,
      new_openings_per_day: 0, new_tactics_per_day: 1, selection: settings.selection,
    });
    assert.equal(saved.status, "tactic_settings");

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
    function evidence() {
      const tables = new Map(databaseRows(database));
      const findings = tables.get("tactical_observations")
        .filter(row => row.dataset_id === dataset && row.current)
        .map(row => ({ ...row, details: JSON.parse(row.details_json) }));
      const analyses = tables.get("tactical_analyses").filter(row => row.dataset_id === dataset);
      const enrollments = new Set(tables.get("tactical_enrollment")
        .filter(row => row.dataset_id === dataset).map(row => row.id));
      const positions = tables.get("tactical_positions").filter(row => enrollments.has(row.enrollment_id));
      const requests = tables.get("tactical_requests").filter(row => row.dataset_id === dataset);
      return { findings, analyses, positions, requests };
    }

    async function completed(total, pending) {
      const deadline = Date.now() + 100000;
      let view;
      do {
        view = evidence();
        assert.ok(view.analyses.every(row => row.status === "complete"), JSON.stringify(view.analyses));
        if (scanResponsiveness === null && view.positions.some(row => row.status === "pending")) {
          const began = performance.now();
          const progress = await api("study-progress", null);
          scanResponsiveness = performance.now() - began;
          assert.ok(progress && !progress.code, JSON.stringify(progress));
          assert.ok(scanResponsiveness < 1000, `Study blocked behind engine analysis for ${scanResponsiveness}ms`);
        }
        if (view.positions.length === total && view.positions.every(row => row.status === "complete")
          && view.requests.filter(row => !row.fulfilled && !row.cancelled).length === pending) return view;
        await new Promise(resolve => setTimeout(resolve, 150));
      } while (Date.now() < deadline);
      assert.fail(`Tactical analysis did not settle: ${JSON.stringify(view)}\n${host.diagnostics()}`);
    }
    const motifs = row => row.details.assessment.personal?.candidates.map(candidate => candidate.motif) ?? [];

    const imported = await api("games/import", { dataset_id: dataset, command_id: "import-five", learner: "Learner", pgn: games });
    assert.equal(imported.new_games, 5, JSON.stringify(imported));
    const history = await completed(5, 1);
    assert.equal(history.analyses.reduce((sum, row) => sum + row.decisions_scanned, 0), 5);
    assert.notEqual(scanResponsiveness, null, "must measure Study while analysis is pending");
    const missed = history.findings.find(row => motifs(row).includes("hanging_material")
      && row.details.assessment.personal.puzzle?.line[0].uci === "d4e5");
    assert.ok(missed);
    assert.equal(missed.ply, 0);
    const training = () => evidence().requests.find(row => row.observation_id === missed.id);
    assert.ok(training());
    assert.equal(training().fulfilled, 0);
    assert.ok(history.findings.some(row => motifs(row).includes("mate_in_one")));
    assert.ok(history.findings.some(row => motifs(row).includes("mate_in_two")));
    const resumed = await api("study", { command: "next" });
    for (const key of ["attempt_id", "target_id", "revision", "display_fen"]) assert.deepEqual(resumed[key], active[key], key);
    blind(resumed);

    const beforeRead = durableTraining(database);
    const retired = validateTacticalHistory(await api("games/tactics", { dataset_id: dataset }), dataset);
    assert.equal(retired.status, "unavailable");
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
      const { chromium } = await import(process.env.TEMPO_PLAYWRIGHT ?? "playwright");
      browser = await chromium.launch({ headless: true,
        ...(process.env.TEMPO_CHROMIUM ? { executablePath: process.env.TEMPO_CHROMIUM } : {}) });
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
    assert.equal(training().fulfilled, 1);
    assert.equal((await client.next()).status, "empty");

    // A subsequent game while idle must become the first card, even though the
    // one-card ordinary daily introduction allowance has already been used.
    const idleGame = await api("games/import", { dataset_id: dataset, command_id: "black-while-idle", learner: "Learner",
      pgn: pgn("Black missed queen", blackFen, "1... h6 1-0", true) });
    assert.equal(idleGame.new_games, 1);
    const idleHistory = await completed(6, 1);
    const blackMiss = idleHistory.findings.find(row => row.id !== missed.id && row.details.fen === blackFen
      && motifs(row).includes("hanging_material"));
    assert.ok(blackMiss);
    assert.ok(idleHistory.requests.some(row => row.observation_id === blackMiss.id && !row.fulfilled && !row.cancelled));
    const first = await client.next();
    blind(first);
    assert.equal(first.learner_color, "black");
    assert.equal(first.display_fen, blackFen);
    t.diagnostic(`Study progress responded in ${scanResponsiveness.toFixed(1)}ms while the Stockfish worker had pending positions.`);
    t.diagnostic(`Verified normal serve, six real-engine games, active-card preservation, durable recovery, next-up and idle-first priority${withBrowser ? ", and actual Study board capture in Chromium" : ""}.`);
  });
