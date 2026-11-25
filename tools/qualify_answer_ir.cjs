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

async function predecessorRecovery(browser, db, accepted) {
  const port = await unusedPort();
  const page = await browser.newPage();
  let original;
  try {
    await serve(predecessor, db, port, async (origin, api) => {
      await seed(api);
      await page.route(answerPattern, async route => {
        if (accepted) assert.equal((await route.fetch()).status(), 200);
        await route.abort();
      });
      await page.goto(origin);
      await reveal(page).click();
      await retry(page).waitFor();
      original = await page.evaluate(() => JSON.parse(localStorage.getItem("tempo.pending-answer.v2")));
      assert.ok(original?.dataset);
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
        JSON.parse(localStorage.getItem("tempo.pending-answer.v2"))), original);
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

(async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "tempo-answer-ir-"));
  let browser;
  try {
    browser = await chromium.launch({ executablePath: chrome, headless: true, timeout: 15000 });
    await predecessorRecovery(browser, path.join(root, "accepted.db"), true);
    await predecessorRecovery(browser, path.join(root, "undelivered.db"), false);
    await twoTabs(browser, path.join(root, "concurrent.db"));
    console.log("Predecessor accepted-lost and undelivered Answers, two-tab recovery, and Next-only retry passed.");
  } finally {
    if (browser) await browser.close();
    fs.rmSync(root, { recursive: true, force: true });
  }
})().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
