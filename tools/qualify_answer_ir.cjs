// NODE_PATH points to an installed playwright-core. Runs only on disposable data.
const { chromium } = require("playwright-core");
const assert = require("node:assert/strict");
const { spawn, execFileSync } = require("node:child_process");
const { once } = require("node:events");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const net = require("node:net");

const [candidate, predecessor] = process.argv.slice(2).map(p => path.resolve(p));
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

async function predecessorRecovery(browser, db, accepted) {
  const port = await unusedPort();
  const page = await browser.newPage();
  let original;
  let previousSchema;
  try {
    await serve(predecessor, db, port, async (origin, api) => {
      await seed(api);
      previousSchema = Number(execFileSync("sqlite3", ["-readonly", db, "PRAGMA user_version"]));
      await page.route(answerPattern, async route => {
        original = route.request().postDataJSON();
        if (accepted) assert.equal((await route.fetch()).status(), 200);
        await route.abort();
      });
      await page.goto(origin);
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
      assert.equal(grades(db), accepted ? 1 : 0);
      await page.unroute(answerPattern);
    });
    execFileSync(candidate, ["migrate-games", db]);
    await serve(candidate, db, port, async () => {
      const recovered = page.waitForRequest(request => request.url().endsWith("/api/study/answer"));
      await page.reload();
      assert.deepEqual((await recovered).postDataJSON(), original);
      await next(page).waitFor();
      assert.equal(grades(db), 1);
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

async function incompleteNextReply(browser, db, automatic) {
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
        await route.fulfill(automatic
          ? { status: 200, json: { ...issued, result: undefined } }
          : { status: 200, contentType: "application/json", body: '{"attempt":' });
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

(async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "tempo-answer-ir-"));
  let browser;
  try {
    browser = await chromium.launch({ executablePath: chrome, headless: true, timeout: 15000 });
    await titleInvariance(browser, root);
    await incompleteReply(browser, path.join(root, "truncated.db"));
    await predecessorRecovery(browser, path.join(root, "accepted.db"), true);
    await predecessorRecovery(browser, path.join(root, "undelivered.db"), false);
    await twoTabs(browser, path.join(root, "concurrent.db"));
    await mixedTabs(browser, path.join(root, "mixed.db"));
    await browserReads(browser, path.join(root, "browse.db"));
    await invalidCurrentReply(browser, path.join(root, "invalid-current.db"));
    await incompleteNextReply(browser, path.join(root, "manual-next.db"), false);
    await incompleteNextReply(browser, path.join(root, "automatic-next.db"), true);
    await incompleteStartupNext(browser, path.join(root, "startup-next.db"));
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
