// NODE_PATH points to an installed playwright-core. Runs only on disposable data.
const { chromium } = require("playwright-core");
const assert = require("node:assert/strict");
const { spawn, execFileSync } = require("node:child_process");
const { once } = require("node:events");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const net = require("node:net");

const [candidate, predecessor, retainedDatabase, packArtifact, legacyStudyClient] = process.argv.slice(2).map(p => path.resolve(p));
const chrome = process.env.TEMPO_QUALIFY_CHROME ||
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
const answerPattern = "**/api/study/answer";
const nextPattern = "**/api/study/next";
const reveal = page => page.getByRole("button", { name: "Reveal answer" });
const next = page => page.getByRole("button", { name: "Next card" });
const retry = page => page.getByRole("button", { name: "Retry", exact: true });

async function unusedPort() {
  const socket = net.createServer().listen(0, "127.0.0.1");
  await once(socket, "listening");
  const { port } = socket.address();
  await new Promise(resolve => socket.close(resolve));
  return port;
}

async function serve(binary, db, port, run) {
  const origin = "http://127.0.0.1:" + port;
  const service = spawn(binary, ["serve", db, String(port), "--no-worker"],
    { stdio: ["ignore", "ignore", "inherit"] });
  const api = async (endpoint, body) => {
    const response = await fetch(origin + "/api/" + endpoint, body === undefined ? {} : {
      method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body),
    });
    if (!response.ok) throw new Error(await response.text());
    return response.json();
  };
  try {
    for (let tries = 0; tries < 150; tries++) {
      if (await api("health").then(() => true).catch(() => false)) break;
      if (tries === 149) throw new Error("Tempo fixture did not start");
      await pause(20);
    }
    return await run(origin, api);
  } finally {
    if (service.exitCode === null && service.signalCode === null) {
      service.kill();
      await once(service, "exit");
    }
  }
}

async function seed(api) {
  for (const line of ["1. e4 e5", "1. d4 d5"]) {
    await api("tactics", { name: line, pgn: line + " 1-0" });
  }
}

function grades(db) {
  return Number(execFileSync("sqlite3", ["-readonly", db,
    "SELECT count(*) FROM attempts WHERE result IS NOT NULL"], { encoding: "utf8" }));
}

async function openPuzzle(api, puzzle, options = {}) {
  const { dataset, revision } = await api("tactics/packs");
  const start = { dataset, revision, puzzle, request: crypto.randomUUID(), practice: false, ...options };
  return { start, play: await api("tactics/plays", start) };
}

const fullLineReveal = page => page.getByRole("button", { name: "Reveal solution", exact: true });

function legacyPuzzleView({ before_fen, ...view }) { return view; }

function puzzleCommandCount(db, intent) {
  const statement = intent.request
    ? `SELECT count(*) FROM lichess_starts WHERE request='${intent.request}'`
    : `SELECT count(*) FROM lichess_moves WHERE play=${intent.play} AND step=${intent.step}`;
  return Number(execFileSync("sqlite3", ["-readonly", db, statement], { encoding: "utf8" }));
}

async function issuePackStudy(api, db) {
  const { play } = await openPuzzle(api, "03N75");
  await api("tactics/plays/move", { dataset: play.dataset, play: play.play, step: play.step, played: null });
  // Isolate which committed card Next issues in this disposable predecessor copy.
  execFileSync("sqlite3", [db, "PRAGMA busy_timeout=2000; UPDATE study SET active=NULL; DELETE FROM deck; " +
    "INSERT INTO deck SELECT 1,card FROM lichess_cards WHERE puzzle='03N75';"]);
  const issued = await api("study/next", { after: null });
  assert.equal(issued.puzzle.puzzle, "03N75");
  return issued;
}

async function oldPuzzleCommand(browser, db, activity, accepted) {
  const port = await unusedPort();
  const context = await browser.newContext();
  const page = await context.newPage();
  const scope = activity === "study_move" ? "study" : "tactics";
  const start = activity === "tactics_start" || activity === "practice_start";
  const endpoint = start ? "tactics/plays" : scope === "study" ? "study/puzzle-move" : "tactics/plays/move";
  const url = "**/api/" + endpoint;
  const legacyKey = `tempo.lichess.v1.${scope}`;
  let intent;
  let beforeFen;
  let previousReceipt;
  let gradesBefore;
  let advancedPractice;
  try {
    await serve(predecessor, db, port, async (origin, api) => {
      if (scope === "study" || activity === "practice_start") {
        beforeFen = (await issuePackStudy(api, db)).puzzle.fen;
      }
      if (activity === "tactics_move") beforeFen = (await openPuzzle(api, "03N75")).play.fen;
      await page.goto(origin);
      if (scope === "tactics") await page.getByRole("button", { name: "Tactics", exact: true }).click();
      let command = fullLineReveal(page);
      if (start) {
        if (activity === "practice_start") {
          const membership = execFileSync("sqlite3", ["-readonly", db,
            "SELECT theme,band,position FROM lichess_members WHERE puzzle='03N75' ORDER BY band,theme LIMIT 1"],
          { encoding: "utf8" }).trim();
          const [theme, band, position] = membership.split("|");
          assert.ok(position);
          await page.getByLabel("Theme").selectOption(theme);
          await page.getByRole("button", { name: new RegExp(`^${band}[–+]`) }).click();
          command = page.getByRole("button", { name: new RegExp(`^Puzzle ${position} .*Practice$`) });
        } else {
          await page.getByLabel("Theme").selectOption("fork");
          await page.getByRole("button", { name: /Open recommended pack/ }).click();
          command = page.getByRole("button", { name: /^(Play next|Resume) puzzle$/ });
        }
      } else if (scope === "tactics") {
        await page.getByRole("button", { name: "Resume 03N75", exact: true }).click();
      }
      gradesBefore = grades(db);
      await page.route(url, async route => {
        intent = route.request().postDataJSON();
        if (accepted) {
          const reply = await route.fetch();
          assert.equal(reply.status(), 200);
          previousReceipt = await reply.json();
        }
        await route.abort();
      }, { times: 1 });
      await command.click();
      await retry(page).waitFor();
      assert.deepEqual(await page.evaluate(key => JSON.parse(sessionStorage.getItem(key)), legacyKey), {
        kind: start ? "start" : "move", intent,
      });
      assert.equal(puzzleCommandCount(db, intent), Number(accepted));
      if (activity === "practice_start" && accepted) {
        const play = await api(endpoint, intent);
        advancedPractice = await api("tactics/plays/move", {
          dataset: play.dataset, play: play.play, step: play.step, played: "d6b8",
        });
        assert.ok(advancedPractice.step > play.step);
      }
    });

    execFileSync(candidate, ["migrate-games", db]);
    await serve(candidate, db, port, async () => {
      const delivered = page.waitForRequest(url);
      const acknowledgement = page.waitForResponse(url);
      await page.reload();
      if (scope === "tactics") await page.getByRole("button", { name: "Tactics", exact: true }).click();
      assert.deepEqual((await delivered).postDataJSON(), intent);
      const reply = await (await acknowledgement).json();
      if (start) {
        assert.equal(reply.before_fen, undefined);
        if (advancedPractice || previousReceipt) assert.deepEqual(reply, advancedPractice ?? previousReceipt);
      } else {
        assert.equal(reply.before_fen, beforeFen);
        if (previousReceipt) assert.deepEqual(legacyPuzzleView(reply), previousReceipt);
      }
      if (start) await fullLineReveal(page).waitFor();
      else await page.getByRole("status").filter({ hasText: "Again" }).waitFor();
      assert.equal(await page.evaluate(key => sessionStorage.getItem(key), legacyKey), null);
      assert.equal(puzzleCommandCount(db, intent), 1);
      assert.equal(grades(db), gradesBefore + Number(!start));
      if (activity === "practice_start") {
        // Practice acceptance records its command, while Learning remains unchanged.
        await fullLineReveal(page).click();
        await page.getByRole("status").filter({ hasText: "Again" }).waitFor();
        assert.equal(grades(db), gradesBefore);
      }
    });
  } finally {
    await context.close();
  }
}

async function explicitActivityAndReopenedPuzzle(browser, db) {
  execFileSync(candidate, ["import-lichess-packs", db, packArtifact]);
  await serve(candidate, db, await unusedPort(), async (origin, api) => {
    const review = await issuePackStudy(api, db);
    assert.equal(review.activity_mode, "full_line");
    assert.equal(review.result, null);
    assert.equal(review.puzzle.before_fen, undefined);
    const context = await browser.newContext();
    let page = await context.newPage();
    try {
      const writes = [];
      page.on("request", request => { if (request.method() === "POST") writes.push(request.url()); });
      for (const change of [
        value => ({ ...value, puzzle: undefined }),
        value => ({ ...value, activity_mode: undefined }),
        value => ({ ...value, activity_mode: "single_answer" }),
      ]) {
        await page.route("**/api/study", async route => {
          const response = await route.fetch();
          await route.fulfill({ response, json: change(await response.json()) });
        }, { times: 1 });
        await page.goto(origin);
        await retry(page).waitFor();
        assert.equal(await fullLineReveal(page).count(), 0);
        assert.equal(await reveal(page).count(), 0);
        assert.deepEqual(writes, []);
      }
      await retry(page).click();
      await fullLineReveal(page).waitFor();

      let intent;
      const url = "**/api/study/puzzle-move";
      await page.route(url, route => {
        intent = route.request().postDataJSON();
        return route.abort();
      }, { times: 1 });
      await fullLineReveal(page).click();
      await retry(page).waitFor();
      assert.equal(puzzleCommandCount(db, intent), 0);
      await page.close();
      page = await context.newPage();
      const replay = page.waitForRequest(url);
      await page.goto(origin);
      assert.deepEqual((await replay).postDataJSON(), intent);
      await page.getByRole("status").filter({ hasText: "Again" }).waitFor();
      assert.equal(puzzleCommandCount(db, intent), 1);
      const calls = [];
      page.on("request", request => { if (request.method() === "POST") calls.push(request.url()); });
      await page.route(nextPattern, route => route.abort(), { times: 1 });
      await page.getByRole("button", { name: "Next", exact: true }).click();
      await retry(page).waitFor();
      await page.getByRole("status").filter({ hasText: "Again" }).waitFor();
      await retry(page).click();
      await retry(page).waitFor({ state: "hidden" });
      assert.deepEqual(calls, [origin + "/api/study/next", origin + "/api/study/next"]);
      assert.equal(puzzleCommandCount(db, intent), 1);
    } finally {
      await context.close();
    }
  });
}

async function impossiblePuzzleAcknowledgements(browser, db) {
  execFileSync(candidate, ["import-lichess-packs", db, packArtifact]);
  await serve(candidate, db, await unusedPort(), async (origin, api) => {
    const { puzzle } = await issuePackStudy(api, db);
    const context = await browser.newContext();
    let page = await context.newPage();
    const url = "**/api/study/puzzle-move";
    const commands = [];
    try {
      await page.goto(origin);
      let submit = fullLineReveal(page);
      for (const change of [
        result => ({ ...result, before_fen: undefined }),
        result => ({ ...puzzle, step: puzzle.step + 2, last_move: result.solution.moves[1] }),
        result => ({ ...result, result: { ...result.result, rating: "Good" },
          step: result.solution.moves.length, last_move: result.solution.moves.at(-1) }),
        result => ({ ...result, step: result.step + 2, last_move: result.solution.moves[1] }),
      ]) {
        await page.route(url, async route => {
          commands.push(route.request().postDataJSON());
          const response = await route.fetch();
          const accepted = await response.json();
          assert.equal(accepted.result.rating, "Again");
          await route.fulfill({ response, json: change(accepted) });
        }, { times: 1 });
        await submit.click();
        await retry(page).waitFor();
        assert.equal(await page.getByRole("status").filter({ hasText: "Again" }).count(), 0);
        assert.equal(await page.getByRole("status").filter({ hasText: "Solved" }).count(), 0);
        assert.deepEqual(commands.at(-1), commands[0]);
        assert.equal(puzzleCommandCount(db, commands[0]), 1);
        submit = retry(page);
      }
      await page.close();
      page = await context.newPage();
      await page.route(url, async route => {
        const response = await route.fetch();
        const receipt = await response.json();
        const white = puzzle.color === "white";
        const otherFen = white
          ? "4k3/8/8/8/8/8/4P3/4K3 w - - 0 1"
          : "4k3/4p3/8/8/8/8/8/4K3 b - - 0 1";
        await route.fulfill({ response, json: { ...receipt, fen: otherFen, before_fen: otherFen,
          solution: { fen: otherFen, moves: [white ? "e2e4" : "e7e5"] } } });
      }, { times: 1 });
      const substituted = page.waitForRequest(url);
      await page.goto(origin);
      assert.deepEqual((await substituted).postDataJSON(), commands[0]);
      await retry(page).waitFor();
      const replay = page.waitForRequest(url);
      await retry(page).click();
      assert.deepEqual((await replay).postDataJSON(), commands[0]);
      await page.getByRole("status").filter({ hasText: "Again" }).waitFor();
      assert.equal(puzzleCommandCount(db, commands[0]), 1);
      assert.equal(grades(db), 2);
    } finally {
      await context.close();
    }
  });
}

async function competingPuzzleTabs(browser, db) {
  execFileSync(candidate, ["import-lichess-packs", db, packArtifact]);
  await serve(candidate, db, await unusedPort(), async (origin, api) => {
    const review = await issuePackStudy(api, db);
    const context = await browser.newContext();
    const older = await context.newPage();
    const newer = await context.newPage();
    const held = Promise.withResolvers();
    const release = Promise.withResolvers();
    const url = "**/api/study/puzzle-move";
    const terminal = page => page.getByRole("status").filter({ hasText: "Again" });
    try {
      await older.goto(origin);
      await newer.goto(origin);
      await fullLineReveal(older).waitFor();
      await fullLineReveal(newer).waitFor();
      let original;
      await older.route(url, async route => {
        original = route.request().postDataJSON();
        const reply = await route.fetch();
        assert.equal(reply.status(), 200);
        const accepted = await reply.json();
        assert.equal(accepted.step, original.step + 2);
        await route.fulfill({ response: reply, json: { ...accepted, step: accepted.step + 2 } });
      }, { times: 1 });
      const board = await older.locator("cg-board").boundingBox();
      const white = review.puzzle.color === "white";
      async function clickSquare(square) {
        const file = square.charCodeAt(0) - 97;
        const rank = Number(square[1]) - 1;
        await older.mouse.click(
          board.x + (0.5 + (white ? file : 7 - file)) * board.width / 8,
          board.y + (0.5 + (white ? 7 - rank : rank)) * board.height / 8,
        );
      }
      await clickSquare("d6");
      await clickSquare("b8");
      await retry(older).waitFor();
      await older.route(url, async route => {
        assert.deepEqual(route.request().postDataJSON(), original);
        const reply = await route.fetch();
        const continued = await reply.json();
        assert.equal(continued.before_fen, review.puzzle.fen);
        await route.fulfill({ response: reply, json: { ...continued, fen: review.puzzle.fen,
          last_move: original.played, legal: review.puzzle.legal } });
      }, { times: 1 });
      await retry(older).click();
      await retry(older).waitFor();
      await older.route(url, async route => {
        const reply = await route.fetch();
        assert.equal(reply.status(), 200);
        held.resolve(route.request().postDataJSON());
        await release.promise;
        await route.fulfill({ response: reply });
      }, { times: 1 });
      await retry(older).click();
      const first = await held.promise;
      assert.deepEqual(first, original);
      assert.equal(first.played, "d6b8");
      assert.equal(puzzleCommandCount(db, first), 1);

      const writes = [];
      newer.on("request", request => {
        if (request.url().endsWith("/api/study/puzzle-move")) writes.push(request.postDataJSON());
      });
      await newer.route("**/api/study", route => route.abort(), { times: 1 });
      await fullLineReveal(newer).click();
      await retry(newer).waitFor();
      assert.equal(writes.length, 1);
      assert.equal(writes[0].step, first.step);
      assert.equal(writes[0].played, null);
      await retry(newer).click();
      await retry(newer).waitFor({ state: "hidden" });
      assert.equal(writes.length, 1, "The known conflict retries Current alone");

      await newer.route(url, route => route.abort(), { times: 1 });
      await fullLineReveal(newer).click();
      await retry(newer).waitFor();
      const last = writes.at(-1);
      assert.ok(last.step > first.step);
      release.resolve();
      await older.waitForFunction(() => [...document.querySelectorAll("button")]
        .some(button => button.textContent === "Reveal solution" && !button.disabled));
      assert.equal(puzzleCommandCount(db, last), 0);
      await newer.reload();
      await terminal(newer).waitFor();
      assert.deepEqual(writes.at(-1), last);
      assert.equal(puzzleCommandCount(db, first), 1);
      assert.equal(puzzleCommandCount(db, last), 1);
      assert.equal(grades(db), 2);
    } finally {
      release.resolve();
      await context.close();
    }
  });
}

async function fullLineRecovery(browser, db) {
  const port = await unusedPort();
  const page = await browser.newPage();
  try {
    await serve(legacyStudyClient, db, port, async (origin, api) => {
      await api("tactics", { name: "Old card", pgn: "1. e4 e5 1-0" });
      await page.goto(origin);
      await reveal(page).click();
      await next(page).waitFor();
    });
    execFileSync(candidate, ["migrate-games", db]);
    execFileSync(candidate, ["import-lichess-packs", db, packArtifact]);
    // Give the repeat deck an empty starting slot on this disposable fixture.
    execFileSync("sqlite3", [db, "DELETE FROM deck"]);
    await serve(candidate, db, port, async (origin, api) => {
      const { play } = await openPuzzle(api, "03N75");
      const move = { dataset: play.dataset, play: play.play, step: 0 };
      await assert.rejects(api("tactics/plays/move", move), /missing field/);
      assert.equal(grades(db), 1);
      await api("tactics/plays/move", { ...move, played: null });
      await next(page).click();
      const rejected = page.waitForResponse(res => res.url().endsWith("/api/study/answer"));
      await reveal(page).click();
      assert.equal((await (await rejected).json()).code, "refresh_required");
      await retry(page).waitFor();
      await page.reload();
      await page.getByRole("button", { name: "Reveal solution" }).waitFor();
      assert.equal(await retry(page).count(), 0);
      assert.equal(grades(db), 2);
      const current = await api("study");
      const studyMove = { dataset: current.dataset, play: current.puzzle.play, step: 0 };
      await assert.rejects(api("study/puzzle-move", studyMove), /missing field/);
      const continued = await api("study/puzzle-move", { ...studyMove, played: "d6b8" });
      assert.equal(continued.result, null);
      const finish = { ...studyMove, step: continued.step, played: "a7b8r" };
      const solved = await api("study/puzzle-move", finish);
      assert.equal(solved.result.rating, "Good");
      assert.equal(solved.solution.moves.at(-1), "a7b8r");
      assert.deepEqual(await api("study/puzzle-move", finish), solved);
      assert.equal(grades(db), 3);
      await page.reload();
      await page.getByRole("status").filter({ hasText: "Solved" }).waitFor();
      const commands = [];
      page.on("request", request => {
        if (request.method() === "POST") commands.push([new URL(request.url()).pathname, request.postDataJSON()]);
      });
      const showSolution = page.getByRole("button", { name: "Show solution", exact: true });
      const backToResult = page.getByRole("button", { name: "Back to result", exact: true });
      await showSolution.focus();
      await page.keyboard.press("Space");
      await backToResult.waitFor();
      assert.equal(await backToResult.evaluate(button => button === document.activeElement), true);
      await page.keyboard.press("Space");
      await showSolution.waitFor();
      assert.equal(await showSolution.evaluate(button => button === document.activeElement), true);
      assert.deepEqual(commands, []);
      await page.route(nextPattern, route => route.abort(), { times: 1 });
      await page.getByRole("button", { name: "Next", exact: true }).click();
      await retry(page).waitFor();
      assert.equal(await page.getByRole("status").filter({ hasText: "Solved" }).count(), 1);
      await retry(page).click();
      await retry(page).waitFor({ state: "hidden" });
      const expectedNext = ["/api/study/next", { after: current.attempt, dataset: current.dataset }];
      assert.deepEqual(commands, [expectedNext, expectedNext]);
      assert.equal(grades(db), 3);
    });
  } finally {
    await page.context().close();
  }
  console.log("A legacy tab's rejected Answer recovered after reload; missing actions produced no grade and the alternate mating finish replayed once.");
}

async function startRecovery(db) {
  let historical, historicalMove, historicalReply, pending, undelivered;
  execFileSync(predecessor, ["import-lichess-packs", db, packArtifact]);
  await serve(predecessor, db, await unusedPort(), async (_, api) => {
    historical = await openPuzzle(api, "03N75");
    historicalMove = { dataset: historical.play.dataset, play: historical.play.play, step: 0, played: "d6b8" };
    historicalReply = await api("tactics/plays/move", historicalMove);
    const { puzzles } = await api("tactics/packs/fork/1000");
    pending = await openPuzzle(api, puzzles.find(row => row.puzzle !== "03N75").puzzle);
    undelivered = { dataset: pending.play.dataset, play: pending.play.play, step: 0, played: null };
  });
  execFileSync(candidate, ["migrate-games", db]);
  await serve(candidate, db, await unusedPort(), async (_, api) => {
    assert.deepEqual(await api("tactics/plays", historical.start), historicalReply);
    const historicalReplay = await api("tactics/plays/move", historicalMove);
    assert.equal(historicalReplay.before_fen, historical.play.fen);
    assert.deepEqual(legacyPuzzleView(historicalReplay), historicalReply);
    const accepted = await api("tactics/plays/move", undelivered);
    assert.equal(accepted.result.rating, "Again");
    assert.deepEqual(await api("tactics/plays/move", undelivered), accepted);
    const current = await openPuzzle(api, historical.start.puzzle);
    assert.equal(current.play.play, historical.play.play);
    await assert.rejects(api("tactics/plays", { ...current.start, revision: "changed" }));
    await assert.rejects(api("tactics/plays", { ...current.start, request: crypto.randomUUID(), revision: "changed" }));
    const finish = await api("tactics/plays/move", { ...historicalMove, step: historicalReply.step, played: "a7b8r" });
    assert.equal(finish.result.rating, "Good");
    assert.deepEqual(await api("tactics/plays", current.start), legacyPuzzleView(finish));
    assert.equal(grades(db), 2);
    const { play: enPassant } = await openPuzzle(api, "07Duy");
    const capture = { dataset: enPassant.dataset, play: enPassant.play, step: enPassant.step };
    await assert.rejects(api("tactics/plays/move", { ...capture, played: "b4c3q" }), /illegal_move/);
    assert.deepEqual(await api(`tactics/plays/${enPassant.play}`), enPassant);
    const legalCapture = await api("tactics/plays/move", { ...capture, played: "b4c3" });
    assert.ok(legalCapture.step > enPassant.step);
  });
  console.log("Schema-12 Start and Move recovery preserved the original play and receipt. New Starts fence their catalog revision.");
}

async function studyDrop(browser, db, { fen, solution, from, to, piece, promotion, correction, retryLost }) {
  await serve(candidate, db, await unusedPort(), async (origin, api) => {
    await api("tactics", { name: "Study drop", pgn: `[SetUp "1"]\n[FEN "${fen}"]\n\n1. ${solution} *` });
    const page = await browser.newPage();
    const answerHeld = Promise.withResolvers();
    const releaseAnswer = Promise.withResolvers();
    const commands = [];
    page.on("request", request => {
      if (request.url().endsWith("/api/study/answer")) commands.push(request.postDataJSON());
    });
    try {
      await page.route(answerPattern, async route => {
        const response = await route.fetch();
        answerHeld.resolve(response.status());
        await releaseAnswer.promise;
        if (retryLost) await route.abort();
        else await route.fulfill({ response });
      }, { times: 1 });
      await page.goto(origin);
      await reveal(page).waitFor();
      await page.waitForFunction(() => !document.querySelector("cg-board piece.anim, cg-board piece.fading"));
      const board = await page.locator("cg-board").boundingBox();
      function center(square) {
        return {
          x: board.x + (square.charCodeAt(0) - 97 + 0.5) * board.width / 8,
          y: board.y + (8 - Number(square[1]) + 0.5) * board.height / 8,
        };
      }
      async function drag(start, end) {
        const a = center(start), b = center(end);
        await page.mouse.move(a.x, a.y);
        await page.mouse.down();
        await page.mouse.move(b.x, b.y, { steps: 12 });
        await page.mouse.up();
      }
      await page.evaluate(({ from, to }) => {
        window.dropFrame = () => [...document.querySelectorAll("cg-board piece")]
          .filter(node => node.cgKey === from || node.cgKey === to)
          .map(node => [node.cgKey, node.cgPiece, node.style.transform]);
        window.dropFrames = [];
        window.recordDrop = () => {
          window.dropFrames.push(window.dropFrame());
          requestAnimationFrame(window.recordDrop);
        };
        // Delay Study seeing the real IndexedDB commit before it can send Answer.
        const transaction = IDBDatabase.prototype.transaction;
        const complete = Object.getOwnPropertyDescriptor(IDBTransaction.prototype, "oncomplete");
        IDBDatabase.prototype.transaction = function (...args) {
          const tx = transaction.apply(this, args);
          if (this.name === "tempo-study-answer-journal" && args[1] === "readwrite") {
            IDBDatabase.prototype.transaction = transaction;
            Object.defineProperty(tx, "oncomplete", {
              set(handler) {
                complete.set.call(tx, event => {
                  window.releaseDropJournal = () => handler.call(tx, event);
                });
              },
            });
          }
          return tx;
        };
      }, { from, to });
      const before = await page.evaluate(() => window.dropFrame());
      await drag(from, "h4");
      await page.waitForFunction(before => JSON.stringify(window.dropFrame()) === JSON.stringify(before), before);
      assert.deepEqual(commands, []);

      await page.evaluate(() => document.addEventListener("mouseup", () => {
        requestAnimationFrame(window.recordDrop);
      }, { once: true }));
      await drag(from, to);
      if (promotion) {
        const queen = page.getByRole("button", { name: "Queen", exact: true });
        await queen.waitFor();
        assert.deepEqual((await page.evaluate(() => window.dropFrame())).map(row => row.slice(0, 2)), [[to, "white pawn"]]);
        await queen.click();
      }
      await page.waitForFunction(() => typeof window.releaseDropJournal === "function");
      assert.deepEqual(commands, [], "Answer was sent before the journal completed");
      await page.waitForFunction(() => window.dropFrames.length >= 2);
      const landed = await page.evaluate(() => window.dropFrame());
      assert.deepEqual(landed.map(row => row.slice(0, 2)), [[to, `white ${piece}`]], "drop reverted while saving Answer");
      function assertLanded(frames) {
        for (const frame of frames) {
          const expected = promotion && frame[0]?.[1] === "white pawn"
            ? [[to, "white pawn", landed[0][2]]] : landed;
          assert.deepEqual(frame, expected, "the dropped piece moved again");
        }
      }

      await page.evaluate(() => window.releaseDropJournal());
      assert.equal(await answerHeld.promise, 200);
      assert.deepEqual(await page.evaluate(() => window.dropFrame()), landed);
      assert.equal(commands[0].played, from + to + (promotion ? "q" : ""));
      assert.equal(grades(db), 1);
      if (correction) assertLanded(await page.evaluate(() => window.dropFrames));
      releaseAnswer.resolve();
      if (retryLost) {
        await retry(page).waitFor();
        assert.deepEqual(await page.evaluate(() => window.dropFrame()), landed);
        await retry(page).click();
      }
      await page.getByRole("status").getByText(correction ? "Again" : "Good", { exact: true }).waitFor();
      if (correction) {
        await page.waitForFunction(square => [...document.querySelectorAll("cg-board piece")]
          .some(node => node.cgKey === square && node.cgPiece === "white rook"), correction);
        const frames = await page.evaluate(() => window.dropFrames);
        assert.ok(!frames.some(frame => frame.some(([square, role]) => square === from && role === "white rook")));
      } else {
        assertLanded(await page.evaluate(() => [...window.dropFrames, window.dropFrame()]));
        assert.deepEqual(await page.evaluate(() => window.dropFrame()), landed);
      }
      assert.deepEqual(commands, retryLost ? [commands[0], commands[0]] : [commands[0]]);
      assert.equal(grades(db), 1);
    } finally {
      releaseAnswer.resolve();
      await page.context().close();
    }
  });
}

function pendingAnswers() {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open("tempo-study-answer-journal", 1);
    request.onerror = () => reject(request.error);
    request.onupgradeneeded = () => request.transaction.abort();
    request.onsuccess = () => {
      const db = request.result;
      const transaction = db.transaction("pending", "readonly");
      const rows = transaction.objectStore("pending").getAll();
      transaction.oncomplete = () => { db.close(); resolve(rows.result); };
      transaction.onabort = () => { db.close(); reject(transaction.error); };
    };
  });
}

async function predecessorRecovery(browser, db, accepted, { retained = false } = {}) {
  const port = await unusedPort();
  const page = await browser.newPage();
  let original;
  let previousSchema;
  let priorGrades;
  try {
    await serve(predecessor, db, port, async (origin, api) => {
      if (!retained) await seed(api);
      priorGrades = grades(db);
      const current = retained ? await api("study") : null;
      previousSchema = Number(execFileSync("sqlite3", ["-readonly", db, "PRAGMA user_version"]));
      await page.route(answerPattern, async route => {
        original = route.request().postDataJSON();
        if (accepted) assert.equal((await route.fetch()).status(), 200);
        await route.abort();
      });
      await page.goto(origin);
      if (retained) {
        if (current?.result) await next(page).click();
        const skip = page.getByRole("button", { name: "Skip to your move" });
        await reveal(page).or(skip).first().waitFor();
        if (await skip.isVisible()) await skip.click();
      }
      await reveal(page).click();
      await retry(page).waitFor();
      assert.ok(original?.dataset);
      if (previousSchema < 7) {
        assert.deepEqual(await page.evaluate(() => JSON.parse(localStorage.getItem("tempo.pending-answer.v2"))), original);
      } else {
        assert.deepEqual(await page.evaluate(pendingAnswers), [
          { version: 1, dataset: original.dataset, attempt: original.attempt, action: { kind: "reveal" } },
        ]);
      }
      assert.equal(grades(db), priorGrades + Number(accepted));
      await page.unroute(answerPattern);
    });
    execFileSync(candidate, ["migrate-games", db]);
    await serve(candidate, db, port, async () => {
      const recovered = page.waitForRequest(request => request.url().endsWith("/api/study/answer"));
      await page.reload();
      assert.deepEqual((await recovered).postDataJSON(), original);
      await next(page).waitFor();
      assert.equal(grades(db), priorGrades + 1);
      assert.deepEqual(await page.evaluate(() =>
        JSON.parse(localStorage.getItem("tempo.pending-answer.v2"))), previousSchema < 7 ? original : null);
    });
  } finally {
    await page.context().close();
  }
}

async function twoTabs(browser, db) {
  const port = await unusedPort();
  await serve(candidate, db, port, async (origin, api) => {
    await seed(api);
    const context = await browser.newContext();
    const older = await context.newPage();
    const newer = await context.newPage();
    const accepted = Promise.withResolvers();
    const releaseOlder = Promise.withResolvers();
    const submitted = Promise.withResolvers();
    const dropNewer = Promise.withResolvers();
    try {
      await older.route(answerPattern, async route => {
        const response = await route.fetch();
        assert.equal(response.status(), 200);
        accepted.resolve(route.request().postDataJSON());
        await releaseOlder.promise;
        await route.fulfill({ response });
      });
      await older.goto(origin);
      await reveal(older).click();
      const earlier = await accepted.promise;
      await newer.goto(origin);
      await next(newer).click();
      await newer.route(answerPattern, async route => {
        submitted.resolve(route.request().postDataJSON());
        await dropNewer.promise;
        await route.abort();
      });
      await reveal(newer).click();
      const later = await submitted.promise;
      assert.notEqual(later.attempt, earlier.attempt);
      releaseOlder.resolve();
      await next(older).waitFor();
      dropNewer.resolve();
      await retry(newer).waitFor();
      assert.equal(grades(db), 1);
      await newer.unroute(answerPattern);
      const replay = newer.waitForRequest(request => request.url().endsWith("/api/study/answer"));
      await newer.reload();
      assert.deepEqual((await replay).postDataJSON(), later);
      await next(newer).waitFor();
      assert.equal(grades(db), 2);

      const commands = [];
      newer.on("request", request => {
        if (request.method() === "POST") commands.push(request.url().split("/").at(-1));
      });
      await newer.route("**/api/study/next", route => route.abort(), { times: 1 });
      await next(newer).click();
      await retry(newer).waitFor();
      await newer.getByRole("status").filter({ hasText: "Again" }).waitFor();
      await retry(newer).click();
      await reveal(newer).waitFor();
      assert.deepEqual(commands, ["next", "next"]);
      assert.equal(grades(db), 2);
      const conflict = await fetch(origin + "/api/study/answer", {
        method: "POST", headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ ...later, played: "e2e4" }),
      });
      assert.equal(conflict.status, 409);
      assert.equal((await conflict.json()).code, "answer_conflict");
      const malformed = await fetch(origin + "/api/study/answer", {
        method: "POST", headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ dataset: later.dataset, attempt: later.attempt }),
      });
      assert.equal(malformed.status, 422);
      assert.equal(grades(db), 2);
    } finally {
      releaseOlder.resolve();
      dropNewer.resolve();
      await context.close();
    }
  });
}

async function incompleteReply(browser, db) {
  const page = await browser.newPage();
  try {
    await serve(candidate, db, await unusedPort(), async (origin, api) => {
      await seed(api);
      let original;
      await page.route(answerPattern, route => {
        original = route.request().postDataJSON();
        return route.fulfill({ json: { rating: "Again" } });
      }, { times: 1 });
      await page.goto(origin);
      await reveal(page).click();
      await retry(page).waitFor();
      assert.equal(grades(db), 0);
      const replay = page.waitForRequest(request => request.url().endsWith("/api/study/answer"));
      await page.reload();
      assert.deepEqual((await replay).postDataJSON(), original);
      await next(page).waitFor();
      assert.equal(grades(db), 1);
    });
  } finally {
    await page.context().close();
  }
}

async function unplayableAnswerReply(browser, db) {
  const page = await browser.newPage();
  try {
    await serve(candidate, db, await unusedPort(), async (origin, api) => {
      await seed(api);
      const commands = [];
      const errors = [];
      page.on("request", request => {
        if (request.url().endsWith("/api/study/answer") || request.url().endsWith("/api/study/next")) {
          commands.push([new URL(request.url()).pathname, request.postDataJSON()]);
        }
      });
      page.on("pageerror", error => errors.push(error.message));
      await page.route(answerPattern, async route => {
        const response = await route.fetch();
        assert.equal(response.status(), 200);
        const result = await response.json();
        assert.equal(result.rating, "Again");
        await route.fulfill({ response, json: { ...result, answers: ["a1a8"], display_move: "a1a8" } });
      }, { times: 1 });
      await page.goto(origin);
      await reveal(page).waitFor();
      commands.length = 0;
      await reveal(page).click();
      await retry(page).waitFor();
      assert.equal(await reveal(page).isVisible(), true);
      const [command] = commands;
      const { dataset, attempt } = command[1];
      assert.deepEqual(await page.evaluate(pendingAnswers), [
        { version: 1, dataset, attempt, action: { kind: "reveal" } },
      ]);
      const effects = () => execFileSync("sqlite3", ["-readonly", db, `
        SELECT id,card_id,submission,result FROM attempts ORDER BY id;
        SELECT * FROM learning ORDER BY card_id;
        SELECT * FROM deck ORDER BY slot;
      `], { encoding: "utf8" });
      const saved = effects();
      assert.equal(grades(db), 1);
      await retry(page).click();
      await next(page).waitFor();
      assert.deepEqual(commands, [command, command]);
      assert.equal(effects(), saved);
      assert.deepEqual(await page.evaluate(pendingAnswers), []);
      assert.deepEqual(errors, []);
    });
  } finally {
    await page.context().close();
  }
}

async function titleInvariance(browser, root) {
  const baseline = path.join(root, "title-baseline.db");
  await serve(candidate, baseline, await unusedPort(), async (_origin, api) => {
    await api("tactics", { name: "Original", pgn: "1. e4 e5 1-0" });
  });
  for (const good of [true, false]) {
    let expected;
    for (const title of ["Original", "Renamed", "", null]) {
      const db = path.join(root, "title-" + good + "-" + title + ".db");
      execFileSync("sqlite3", [baseline, ".backup " + db]);
      execFileSync("sqlite3", [db, "UPDATE cards SET title='" + (title ?? "Original") + "'"]);
      const page = await browser.newPage();
      try {
        await serve(candidate, db, await unusedPort(), async (origin) => {
          const commands = [];
          page.on("request", request => {
            if (request.method() === "POST") {
              commands.push([new URL(request.url()).pathname, request.postDataJSON()]);
            }
          });
          if (title === null) await page.route(nextPattern, async route => {
            const response = await route.fetch();
            const view = await response.json();
            if (view) delete view.title;
            await route.fulfill({ response, json: view });
          });
          await page.goto(origin);
          await reveal(page).waitFor();
          if (good) {
            const board = await page.locator("cg-board").boundingBox();
            const x = board.x + 4.5 * board.width / 8;
            await page.mouse.click(x, board.y + 6.5 * board.height / 8);
            await page.mouse.click(x, board.y + 4.5 * board.height / 8);
            await page.getByText("No cards are ready.", { exact: false }).waitFor();
          } else {
            await reveal(page).click();
            await next(page).click();
            await reveal(page).waitFor();
          }
          const effects = execFileSync("sqlite3", ["-readonly", db, `
            SELECT id,card_id,submission,result FROM attempts ORDER BY id;
            SELECT card_id,stability,difficulty,due_at-last_at FROM learning ORDER BY card_id;
            SELECT slot,card_id FROM deck ORDER BY slot;
            SELECT active,last_kind FROM study;
          `], { encoding: "utf8" });
          const actual = { commands, effects };
          if (expected) assert.deepEqual(actual, expected);
          expected = actual;
        });
      } finally {
        await page.context().close();
      }
    }
    assert.equal(expected.commands.filter(([route]) => route.endsWith("/answer")).length, 1);
  }
  console.log("Mounted Study: original, renamed, blank and omitted titles preserved Good/Again commands, receipts, schedules and continuation.");
}
async function mixedTabs(browser, db) {
  const port = await unusedPort();
  const context = await browser.newContext();
  const old = await context.newPage();
  const current = await context.newPage();
  const accepted = Promise.withResolvers();
  const releaseOld = Promise.withResolvers();
  const submitted = Promise.withResolvers();
  const dropCurrent = Promise.withResolvers();
  try {
    await serve(predecessor, db, port, async (origin, api) => {
      await seed(api);
      await old.route(answerPattern, async route => {
        const response = await route.fetch();
        assert.equal(response.status(), 200);
        accepted.resolve(route.request().postDataJSON());
        await releaseOld.promise;
        await route.fulfill({ response });
      });
      await old.goto(origin);
      await reveal(old).click();
      await accepted.promise;
    });
    const previous = await accepted.promise;
    execFileSync(candidate, ["migrate-games", db]);
    await serve(candidate, db, port, async (origin) => {
      await current.goto(origin);
      await current.reload();
      assert.match(await current.locator("script[type=module]").getAttribute("src"), /\?v=/);
      await next(current).click();
      await current.route(answerPattern, async route => {
        submitted.resolve(route.request().postDataJSON());
        await dropCurrent.promise;
        await route.abort();
      });
      await reveal(current).click();
      const later = await submitted.promise;
      assert.notEqual(later.attempt, previous.attempt);
      releaseOld.resolve();
      await next(old).waitFor();
      assert.equal(await old.evaluate(() => localStorage.getItem("tempo.pending-answer.v2")), null);
      dropCurrent.resolve();
      await retry(current).waitFor();
      await current.unroute(answerPattern);
      const replay = current.waitForRequest(request => request.url().endsWith("/api/study/answer"));
      await current.reload();
      assert.deepEqual((await replay).postDataJSON(), later);
      await next(current).waitFor();
      assert.equal(grades(db), 2);
    });
  } finally {
    releaseOld.resolve();
    dropCurrent.resolve();
    await context.close();
  }
  console.log("A still-loaded predecessor tab finished its Answer after cutover while the new tab retained and replayed its newer Answer.");
}

async function invalidCurrentReply(browser, db) {
  await serve(candidate, db, await unusedPort(), async (origin, api) => {
    await seed(api);
    const page = await browser.newPage();
    const commands = [];
    try {
      page.on("request", request => {
        if (request.method() === "POST") commands.push(request.url().split("/").at(-1));
      });
      await page.route("**/api/study", route => route.fulfill({
        status: 200, contentType: "application/json", body: "{"
      }), { times: 1 });
      await page.goto(origin);
      await retry(page).waitFor();
      assert.deepEqual(commands, []);
      await retry(page).click();
      await reveal(page).waitFor();
      assert.deepEqual(commands, ["next"]);

      await page.route(answerPattern, route => route.fulfill({
        status: 409, json: { code: "answer_conflict", error: "Already resolved" }
      }), { times: 1 });
      await page.route("**/api/study", route => route.fulfill({
        status: 503, json: { code: "view_unavailable", error: "Study view busy" }
      }), { times: 1 });
      await reveal(page).click();
      await retry(page).waitFor();
      await retry(page).click();
      await retry(page).waitFor({ state: "hidden" });
      assert.deepEqual(commands, ["next", "answer"]);
      assert.equal(grades(db), 0);
    } finally {
      await page.context().close();
    }
  });
}

async function incompleteNextReply(browser, db, failure) {
  const automatic = failure === "missing-result";
  await serve(candidate, db, await unusedPort(), async (origin, api) => {
    await seed(api);
    const page = await browser.newPage();
    const commands = [];
    let issued;
    try {
      await page.goto(origin);
      await reveal(page).waitFor();
      const first = await api("study");
      page.on("request", request => {
        if (request.method() === "POST") {
          commands.push([request.url().split("/").at(-1), request.postDataJSON()]);
        }
      });
      await page.route(nextPattern, async route => {
        const response = await route.fetch();
        assert.equal(response.status(), 200);
        issued = await response.json();
        assert.notEqual(issued.attempt, first.attempt);
        if (failure === "truncated") {
          await route.fulfill({ status: 200, contentType: "application/json", body: '{"attempt":' });
          return;
        }
        if (failure === "unplayable-input" || failure === "no-input") {
          const legal = failure === "no-input" ? [] : [...issued.legal, "a1a8"];
          await route.fulfill({ status: 200, json: { ...issued, legal } });
          return;
        }
        const result = failure === "unplayable-answer"
          ? { rating: "Again", answers: ["a1a8"], repeat_after: null }
          : undefined;
        await route.fulfill({ status: 200, json: { ...issued, result } });
      }, { times: 1 });
      const rating = automatic ? "Good" : "Again";
      if (automatic) {
        const saved = execFileSync("sqlite3", ["-readonly", db,
          "SELECT cards.answers FROM study JOIN attempts ON attempts.id=study.active " +
          "JOIN cards ON cards.id=attempts.card_id"], { encoding: "utf8" });
        const [move] = JSON.parse(saved);
        const board = await page.locator("cg-board").boundingBox();
        const click = (file, rank) => page.mouse.click(
          board.x + (file.charCodeAt(0) - 97 + 0.5) * board.width / 8,
          board.y + (8 - Number(rank) + 0.5) * board.height / 8,
        );
        await click(move[0], move[1]);
        await click(move[2], move[3]);
      } else {
        await reveal(page).click();
        await next(page).click();
      }
      await retry(page).waitFor();
      await page.getByRole("status").getByText(rating, { exact: true }).waitFor();
      assert.equal(await page.getByText("No cards are ready.", { exact: false }).count(), 0);
      assert.equal((await api("study")).attempt, issued.attempt);
      const replayed = page.waitForResponse(response => response.url().endsWith("/api/study/next"));
      await retry(page).click();
      assert.equal((await (await replayed).json()).attempt, issued.attempt);
      await reveal(page).waitFor();
      assert.deepEqual(commands.map(([command]) => command), ["answer", "next", "next"]);
      assert.deepEqual(commands[1][1], { after: first.attempt, dataset: first.dataset });
      assert.deepEqual(commands[1], commands[2]);
      assert.equal(grades(db), 1);
      assert.equal(Number(execFileSync("sqlite3", ["-readonly", db, "SELECT count(*) FROM attempts"])), 2);
    } finally {
      await page.close();
    }
  });
}

async function incompleteStartupNext(browser, db) {
  await serve(candidate, db, await unusedPort(), async origin => {
    const page = await browser.newPage();
    const calls = [];
    try {
      page.on("request", request => {
        if (request.url().endsWith("/api/study")) calls.push(["current"]);
        if (request.url().endsWith("/api/study/next")) calls.push(["next", request.postDataJSON()]);
        if (request.url().endsWith("/api/study/answer")) calls.push(["answer"]);
      });
      await page.route(nextPattern, route => route.fulfill({
        status: 200, contentType: "application/json", body: "",
      }), { times: 1 });
      await page.goto(origin);
      await retry(page).waitFor();
      assert.equal(await page.getByText("No cards are ready.", { exact: false }).count(), 0);
      await retry(page).click();
      await page.getByText("No cards are ready.", { exact: false }).waitFor();
      assert.deepEqual(calls, [["current"], ["next", { after: null }], ["next", { after: null }]]);
    } finally {
      await page.close();
    }
  });
}

async function browserReads(browser, db) {
  await serve(candidate, db, await unusedPort(), async origin => {
    const page = await browser.newPage();
    const firstList = Promise.withResolvers();
    const releaseFirst = Promise.withResolvers();
    const statusRefresh = Promise.withResolvers();
    let lists = 0;
    let statuses = 0;
    let posts = 0;
    const row = (id, white) => ({
      id, white, black: "Opponent", learner_color: "white",
      route: { fen: "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1", moves: ["e2e4"] }
    });
    try {
      await page.route("**/api/games/jobs", async route => {
        if (route.request().method() === "POST") {
          posts++;
          await route.fulfill({ json: { id: 1 } });
          return;
        }
        const added = ++statuses;
        await route.fulfill({ json: [{ id: 1, operation: "all", provider: "lichess", status: "running", added }] });
        if (added === 2) statusRefresh.resolve();
      });
      await page.route("**/api/games", async route => {
        const read = ++lists;
        if (read === 1) {
          firstList.resolve();
          await releaseFirst.promise;
          await route.fulfill({ json: [row("old", "Obsolete")] });
        } else if (read === 3) {
          await route.fulfill({ status: 503, json: { code: "view_unavailable", error: "Browse busy" } });
        } else {
          await route.fulfill({ json: [row("new", "Current")] });
        }
      });
      await page.goto(origin);
      await page.getByRole("button", { name: "Games", exact: true }).click();
      await firstList.promise;
      await page.getByText("1 added", { exact: false }).waitFor();
      await page.getByRole("button", { name: "Import all games" }).first().click();
      await statusRefresh.promise;
      await page.getByText("2 added", { exact: false }).waitFor();
      assert.equal(lists, 1, "status invalidation started a concurrent archive request");
      releaseFirst.resolve();
      await page.getByRole("button", { name: "Current vs Opponent" }).waitFor();
      assert.equal(lists, 2);
      assert.equal(await page.getByRole("button", { name: "Obsolete vs Opponent" }).count(), 0);
      await page.getByRole("button", { name: "Import all games" }).first().click();
      const retryList = page.getByRole("button", { name: "Retry list" });
      await retryList.waitFor();
      assert.equal(await page.getByRole("button", { name: "Current vs Opponent" }).count(), 1);
      await retryList.click();
      await retryList.waitFor({ state: "hidden" });
      assert.equal(lists, 4);
      assert.equal(posts, 2);
    } finally {
      releaseFirst.resolve();
      await page.context().close();
    }
  });
  console.log("Mounted Games coalesced status invalidation, retained its page on failure, and retried the read alone.");
}

async function openingPreview(browser, db) {
  const page = await browser.newPage();
  try {
    await serve(candidate, db, await unusedPort(), async (origin) => {
      const errors = [];
      page.on("pageerror", error => errors.push(error.message));
      await page.goto(origin);
      await page.getByRole("button", { name: "Openings", exact: true }).click();
      await page.getByText("Start from PGN or FEN", { exact: true }).click();
      const input = page.getByRole("textbox", { name: "Opening PGN or FEN" });
      const useLine = page.getByRole("button", { name: "Use these moves as the line" });
      const previewPattern = "**/api/openings/pgn";
      async function holdPreview(pgn) {
        const reply = Promise.withResolvers();
        await page.route(previewPattern, async route => {
          try {
            const response = await route.fetch();
            assert.equal(response.status(), 200);
            reply.resolve(() => route.fulfill({ response }));
          } catch (error) {
            reply.reject(error);
          }
        }, { times: 1 });
        await input.fill(pgn);
        await useLine.click();
        return reply.promise;
      }

      const stale = await holdPreview("1. c4 e5 1-0");
      const canceled = page.waitForEvent("requestfailed", {
        predicate: request => request.url().endsWith("/api/openings/pgn"), timeout: 5000,
      });
      await input.fill("rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1");
      await page.getByRole("button", { name: "Load position", exact: true }).click();
      await canceled;
      await stale().catch(() => {});
      assert.equal(await page.getByText("0 continuation plies", { exact: true }).isVisible(), true);

      const olderPgn = "1. c4 e5 1-0";
      const newerPgn = "1. d4 d5 1-0";
      await page.evaluate(() => {
        const text = File.prototype.text;
        File.prototype.text = function () {
          return new Promise((resolve, reject) => {
            window.finishOpeningFileRead = async (fail) => {
              delete window.finishOpeningFileRead;
              if (fail) reject(new Error("Superseded file failure"));
              else resolve(await text.call(this));
            };
          });
        };
      });
      async function selectOpeningFile(name) {
        await page.getByLabel("Upload opening PGN").setInputFiles({
          name, mimeType: "text/plain", buffer: Buffer.from(olderPgn),
        });
        await page.waitForFunction(() => typeof window.finishOpeningFileRead === "function");
      }

      await selectOpeningFile("old.pgn");
      const current = await holdPreview(newerPgn);
      await page.evaluate(() => window.finishOpeningFileRead(false));
      assert.equal(await input.inputValue(), newerPgn);
      await page.getByRole("textbox", { name: "Opening name" }).fill("Edited name");
      await current();
      const draft = page.getByText("2 continuation plies", { exact: true });
      await draft.waitFor();

      await selectOpeningFile("failed.pgn");
      await input.fill("1. e4 e5 1-0");
      await page.evaluate(() => window.finishOpeningFileRead(true));
      assert.equal(await page.getByText("Superseded file failure").isVisible(), false);
      await selectOpeningFile("current.pgn");
      await page.evaluate(() => window.finishOpeningFileRead(false));
      assert.equal(await input.inputValue(), olderPgn);

      const initialFen = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
      for (const malformed of [
        { fen: "invalid", moves: [] },
        { fen: initialFen, moves: ["a1a8"] },
        { fen: initialFen, moves: ["e2e4garbage"] },
      ]) {
        await page.route(previewPattern, route => route.fulfill({ json: malformed }), { times: 1 });
        const reply = page.waitForResponse(response => response.url().endsWith("/api/openings/pgn"));
        await useLine.click();
        await reply;
        await page.getByText("The opening preview reply was incomplete. Try again.").waitFor();
        assert.equal(await draft.isVisible(), true);
      }
      const query = sql => execFileSync("sqlite3", ["-readonly", db, sql], { encoding: "utf8" }).trim();
      assert.equal(query("SELECT count(*) FROM openings"), "0");
      await page.getByRole("button", { name: "Save opening" }).click();
      await page.getByText("Opening saved. Its responses are ready in Study.", { exact: true }).waitFor();
      assert.equal(query("SELECT name FROM openings"), "Edited name");
      assert.deepEqual(JSON.parse(query("SELECT route FROM openings")).moves, ["d2d4", "d7d5"]);
      assert.deepEqual(errors, []);
    });
  } finally {
    await page.context().close();
  }
}

(async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "tempo-answer-ir-"));
  let browser;
  try {
    browser = await chromium.launch({ executablePath: chrome, headless: true, timeout: 15000 });
    if (process.env.TEMPO_QUALIFY_PUZZLE_JOURNAL === "1") {
      assert.ok(retainedDatabase && packArtifact, "Provide the installed predecessor database and pack artifact");
      await explicitActivityAndReopenedPuzzle(browser, path.join(root, "puzzle-reopen.db"));
      await impossiblePuzzleAcknowledgements(browser, path.join(root, "puzzle-invalid-acks.db"));
      await competingPuzzleTabs(browser, path.join(root, "puzzle-tabs.db"));
      for (const activity of ["study_move", "tactics_move", "tactics_start", "practice_start"]) {
        for (const accepted of [true, false]) {
          const copy = path.join(root, `${activity}-${accepted}.db`);
          execFileSync("sqlite3", ["-readonly", retainedDatabase, ".backup " + copy]);
          await oldPuzzleCommand(browser, copy, activity, accepted);
        }
      }
      console.log("Full-line mode, tab reopen, Next-only retry and both predecessor Move and Start delivery states passed.");
      return;
    }
    if (process.env.TEMPO_QUALIFY_PACKS_ONLY === "1") {
      assert.ok(retainedDatabase && packArtifact && legacyStudyClient, "Provide all predecessor inputs");
      for (const accepted of [true, false]) {
        const copy = path.join(root, `retained-${accepted}.db`);
        execFileSync("sqlite3", ["-readonly", retainedDatabase, ".backup " + copy]);
        await predecessorRecovery(browser, copy, accepted, { retained: true });
      }
      await fullLineRecovery(browser, path.join(root, "legacy-full-line.db"));
      await startRecovery(path.join(root, "pack-schema12.db"));
      return;
    }
    const rookFen = "4k3/n7/8/8/8/8/8/R3K3 w - - 0 1";
    for (const [name, move] of Object.entries({
      capture: { fen: rookFen, solution: "Rxa7", from: "a1", to: "a7", piece: "rook", retryLost: true },
      quiet: { fen: rookFen, solution: "Ra2", from: "a1", to: "a2", piece: "rook" },
      correction: { fen: rookFen, solution: "Rxa7", from: "a1", to: "a2", piece: "rook", correction: "a7" },
      promotion: { fen: "r3k3/1P6/8/8/8/8/8/4K3 w - - 0 1", solution: "bxa8=Q+", from: "b7", to: "a8", piece: "queen", promotion: true },
    })) {
      await studyDrop(browser, path.join(root, `drop-${name}.db`), move);
    }
    console.log("Study drops stayed in place through journal, response and exact Answer retry. Quiet, illegal, correction and promotion paths passed.");
    await titleInvariance(browser, root);
    await incompleteReply(browser, path.join(root, "truncated.db"));
    await unplayableAnswerReply(browser, path.join(root, "unplayable-answer.db"));
    await predecessorRecovery(browser, path.join(root, "accepted.db"), true);
    await predecessorRecovery(browser, path.join(root, "undelivered.db"), false);
    if (retainedDatabase) {
      for (const accepted of [true, false]) {
        const copy = path.join(root, `retained-${accepted}.db`);
        execFileSync("sqlite3", ["-readonly", retainedDatabase, ".backup " + copy]);
        await predecessorRecovery(browser, copy, accepted, { retained: true });
      }
      console.log("Both predecessor Answer deliveries recovered on copies of its retained database.");
    }
    if (packArtifact && legacyStudyClient) {
      await fullLineRecovery(browser, path.join(root, "legacy-full-line.db"));
      await startRecovery(path.join(root, "pack-schema12.db"));
    }
    await twoTabs(browser, path.join(root, "concurrent.db"));
    await mixedTabs(browser, path.join(root, "mixed.db"));
    await browserReads(browser, path.join(root, "browse.db"));
    await invalidCurrentReply(browser, path.join(root, "invalid-current.db"));
    for (const failure of ["truncated", "missing-result", "unplayable-answer", "unplayable-input", "no-input"]) {
      await incompleteNextReply(browser, path.join(root, failure + "-next.db"), failure);
    }
    await incompleteStartupNext(browser, path.join(root, "startup-next.db"));
    await openingPreview(browser, path.join(root, "preview.db"));
    console.log("Malformed Answer feedback retained the original journal and one durable grade. Superseded files and invalid previews kept the draft intact, and name edits left the current preview applicable.");
    console.log("Malformed Next replies preserved saved manual and automatic results. Retry reused the issued attempt and the startup retry accepted real null.");
    console.log("Predecessor accepted-lost and undelivered Answers, two-tab recovery, incomplete reply replay, and Next-only retry passed.");
  } finally {
    if (browser) await browser.close();
    fs.rmSync(root, { recursive: true, force: true });
  }
})().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
