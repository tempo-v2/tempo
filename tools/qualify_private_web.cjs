// Qualify the packaged surface on a disposable copy with an unanswered
// single-answer Study attempt and imported packs. Supply BINARY and SOURCE_DB.
const assert = require("node:assert/strict");
const { execFileSync, spawn } = require("node:child_process");
const { once } = require("node:events");
const { mkdtemp, readFile, rm } = require("node:fs/promises");
const { createServer, request } = require("node:http");
const { tmpdir } = require("node:os");
const path = require("node:path");
const { createInterface } = require("node:readline");
const { setTimeout: delay } = require("node:timers/promises");
const { chromium, webkit } = require("playwright-core");

const [binary, source] = process.argv.slice(2);
assert.ok(binary && source, "Provide the built Tempo binary and a retained fixture database");
const host = "lean.example.ts.net:10000";
const origin = "https://" + host;
const upstream = "http://127.0.0.1:41824";
const login = "owner@example.com";
const serveHeaders = { host, "x-forwarded-host": host, "x-forwarded-proto": "https", "tailscale-user-login": login };
const button = (page, name) => page.getByRole("button", { name, exact: true });
const fixture = name => readFile(path.join(__dirname, "../tests/fixtures", name), "utf8");
const sql = (db, query) => execFileSync("sqlite3", ["-readonly", db, query], { encoding: "utf8" }).trim();

async function settle(page, endpoint, send, accepted) {
  const pattern = "**/api/" + endpoint;
  let command;
  await page.route(pattern, async route => {
    command = route.request().postDataJSON();
    if (accepted) {
      const response = await route.fetch();
      assert.equal(response.status(), 200, await response.text());
    }
    await route.abort();
  }, { times: 1 });
  await send();
  return () => command;
}

async function clickMove(page, move, color, phone) {
  const board = page.locator("cg-board");
  await board.scrollIntoViewIfNeeded();
  const box = await board.boundingBox();
  for (const square of [move.slice(0, 2), move.slice(2, 4)]) {
    const file = square.charCodeAt(0) - 97;
    const rank = Number(square[1]) - 1;
    const x = box.x + (color === "white" ? file + .5 : 7.5 - file) * box.width / 8;
    const y = box.y + (color === "white" ? 7.5 - rank : rank + .5) * box.height / 8;
    if (phone) await page.touchscreen.tap(x, y);
    else await page.mouse.click(x, y);
  }
}

async function journey(db, desktop, web, phone) {
  const engine = phone ? webkit : chromium;
  const browser = await engine.launch(phone ? {} : { executablePath: process.env.TEMPO_QUALIFY_CHROME });
  try {
    const viewport = phone ? { width: 390, height: 844 } : { width: 1200, height: 850 };
    const page = await browser.newPage({ viewport, hasTouch: phone, isMobile: phone });
    const press = locator => phone ? locator.tap() : locator.click();
    const faults = [];
    page.on("pageerror", error => faults.push(error.message));
    const local = await browser.newPage({ hasTouch: phone });
    await local.goto(desktop);
    assert.deepEqual(await local.locator("header nav button").allTextContents(), ["Study", "Openings", "Tactics", "Games", "Settings"]);
    const initial = await fetch(desktop + "/api/study").then(response => response.json());
    assert.equal(initial.activity_mode, "single_answer");
    assert.equal(initial.result, null);
    const grades = () => Number(sql(db, "SELECT count(*) FROM attempts WHERE result IS NOT NULL"));
    const first = grades();

    const study = page.waitForResponse(reply => reply.url() === web + "/api/study");
    await page.bringToFront();
    await page.goto(web);
    assert.equal((await (await study).json()).attempt, initial.attempt);
    assert.deepEqual(await page.locator("header nav button").allTextContents(), ["Study", "Tactics"]);
    assert.equal(await button(page, "Review after answer").count(), 0);
    await button(page, "Reveal answer").waitFor();
    const answer = await settle(page, "study/answer", () => press(button(page, "Reveal answer")), phone);
    const retry = button(page, "Retry");
    await retry.waitFor();
    assert.equal(grades(), first + Number(phone));
    const replay = page.waitForRequest(req => req.url().endsWith("/api/study/answer"));
    await press(retry);
    assert.deepEqual((await replay).postDataJSON(), answer());
    await page.getByRole("status").filter({ hasText: "Again" }).waitFor();
    assert.equal(grades(), first + 1);
    assert.equal(await button(page, "Review move").count(), 0);

    await page.route("**/api/study/next", route => route.abort(), { times: 1 });
    await press(button(page, "Next card"));
    await retry.waitFor();
    await page.getByRole("status").filter({ hasText: "Again" }).waitFor();
    const nextWrite = page.waitForRequest(req => req.method() === "POST");
    await press(retry);
    assert.equal(new URL((await nextWrite).url()).pathname, "/api/study/next");
    await retry.waitFor({ state: "hidden" });
    assert.equal(grades(), first + 1);

    await press(button(page, "Tactics"));
    await press(page.locator(".pack-header").first());
    const start = page.waitForResponse(reply => reply.url().endsWith("/api/tactics/plays") && reply.request().method() === "POST");
    await press(page.getByRole("button", { name: /^(Play next|Resume) puzzle$/ }));
    let view = await (await start).json();
    assert.ok(view.play, JSON.stringify(view));
    const solution = JSON.parse(sql(db, `SELECT v.moves FROM lichess_plays p JOIN lichess_versions v
      ON v.puzzle=p.puzzle AND v.version=p.version WHERE p.id=${Number(view.play)}`));
    while (!view.result) {
      const response = page.waitForResponse(reply => reply.url().endsWith("/api/tactics/plays/move"));
      await clickMove(page, solution[view.step], view.color, phone);
      view = await (await response).json();
      assert.ok(view.play, JSON.stringify(view));
    }
    assert.equal(view.result.rating, "Good");
    await page.getByRole("status").filter({ hasText: "Solved" }).waitFor();
    if (phone) {
      const board = await page.locator("cg-board").boundingBox();
      const controls = await page.locator(".study-panel").boundingBox();
      assert.ok(board.x + board.width <= viewport.width && board.y + board.height <= controls.y);
    }
    await press(button(page, "Show solution"));
    await button(page, "Close solution").or(button(page, "Back to result")).waitFor();

    await press(page.locator("header").getByRole("button", { name: "Capture", exact: true }));
    const dialog = page.getByRole("dialog", { name: "Capture puzzle" });
    const input = dialog.getByRole("textbox", { name: "Puzzle PGN or Lichess puzzle ID" });
    const save = () => press(button(dialog, "Save puzzle"));
    const pgn = await fixture("chesscom-2293026.pgn");
    await input.fill(pgn);
    await press(dialog.getByText("Import options", { exact: true }));
    await dialog.locator('select:has(option[value="chess_com_puzzle"]:checked)').waitFor();
    assert.equal(await dialog.getByLabel("First move is setup").isChecked(), true);
    const before = Number(sql(db, "SELECT count(*) FROM tactic_capture_receipts"));
    const capture = await settle(page, "tactics/capture", save, !phone);
    const retryCapture = button(dialog, "Retry save");
    await retryCapture.waitFor();
    assert.equal(Number(sql(db, "SELECT count(*) FROM tactic_capture_receipts")), before + Number(!phone));
    assert.equal(await input.inputValue(), pgn);
    const captured = page.waitForResponse(reply => reply.url().endsWith("/api/tactics/capture"));
    await press(retryCapture);
    const capturedReply = await captured;
    assert.deepEqual(capturedReply.request().postDataJSON(), capture());
    const { id } = await capturedReply.json();
    await dialog.getByText("Saved", { exact: true }).waitFor();
    assert.equal(Number(sql(db, "SELECT count(*) FROM tactic_capture_receipts")), before + 1);
    assert.equal(sql(db, `SELECT count(*) FROM tactics WHERE id='${id}'`), "1");
    await dialog.getByLabel("Upload puzzle PGN").setInputFiles({
      name: "lichess.pgn", mimeType: "application/x-chess-pgn", buffer: Buffer.from(await fixture("lichess-capture-chapter.pgn")),
    });
    const chapter = page.waitForResponse(reply => reply.url().endsWith("/api/tactics/capture"));
    await save();
    assert.equal((await chapter).status(), 200);
    await dialog.getByText("Saved", { exact: true }).waitFor();
    if (phone) assert.ok((await dialog.boundingBox()).width <= viewport.width);
    if (!phone) {
      await input.fill("PSjmf");
      const lookup = page.waitForResponse(reply => reply.url().endsWith("/api/tactics/capture/lichess"));
      await save();
      const result = await lookup;
      assert.equal(result.status(), 200, await result.text());
      assert.equal((await result.json()).puzzle.source_url, "https://lichess.org/training/PSjmf");
      await dialog.getByText("Saved", { exact: true }).waitFor();
    }
    await press(button(dialog, "Close"));
    await local.bringToFront();
    await press(button(local, "Tactics"));
    const libraryLoad = local.waitForResponse(reply => reply.url() === desktop + "/api/tactics");
    await press(button(local, "Puzzle library"));
    const savedPuzzle = button(local, "Chess.com puzzle 2293026");
    let rows = await (await libraryLoad).json();
    while (!rows.some(puzzle => puzzle.id === id) && rows.length === 100) {
      const more = local.waitForResponse(reply => reply.url().includes("/api/tactics?after="));
      await press(button(local, "Load more"));
      rows = await (await more).json();
    }
    assert.ok(rows.some(puzzle => puzzle.id === id));
    await press(savedPuzzle);
    await local.getByRole("link", { name: "Open on Chess.com" }).waitFor();
    assert.deepEqual(faults, []);
  } finally {
    await browser.close();
  }
}

async function accessChecks(db) {
  async function status(route, headers, method = "GET") {
    return new Promise((resolve, reject) => {
      const req = request(upstream + route, { method, headers }, response => {
        response.resume();
        resolve(response.statusCode);
      });
      req.on("error", reject);
      req.end();
    });
  }
  assert.equal(await status("/", serveHeaders), 200);
  assert.equal(await status("/assets/app.js", { ...serveHeaders, "tailscale-user-login": "other@example.com" }), 403);
  for (const wrong of [
    { host: "wrong.example.ts.net:10000" }, { "x-forwarded-host": "wrong" },
    { "x-forwarded-proto": "http" }, { "tailscale-funnel-request": "true" },
    { "tailscale-user-login": [login, login] }, { origin: "https://other.example" },
  ]) assert.equal(await status("/api/health", { ...serveHeaders, ...wrong }), 403);
  const missing = { ...serveHeaders };
  delete missing["tailscale-user-login"];
  assert.equal(await status("/", missing), 403);
  const before = sql(db, "SELECT count(*) FROM tactic_capture_receipts");
  for (const route of ["study/answer", "tactics/import-options", "tactics/capture", "tactics/capture/lichess"]) {
    assert.equal(await status("/api/" + route, serveHeaders, "POST"), 403);
    assert.equal(await status("/api/" + route, { ...serveHeaders, origin: "https://other.example" }, "POST"), 403);
  }
  assert.equal(sql(db, "SELECT count(*) FROM tactic_capture_receipts"), before);
  for (const route of ["games", "settings", "openings", "coverage"]) {
    assert.equal(await status("/api/" + route, serveHeaders), 404);
  }
  assert.equal(await status("/api/openings/study-review/apply", { ...serveHeaders, origin }, "POST"), 404);
  assert.equal(await status("/api/tactics", { ...serveHeaders, origin }, "POST"), 405);
}

async function run(phone) {
  const root = await mkdtemp(path.join(tmpdir(), "tempo-private-web-"));
  const db = path.join(root, "copy.sqlite3");
  let server;
  let proxy;
  try {
    execFileSync("sqlite3", ["-readonly", source, ".backup " + db]);
    server = spawn(binary, ["serve", db, "0", "--no-worker"], {
      env: { ...process.env, TEMPO_LEAN_WEB_ORIGIN: origin, TEMPO_LEAN_WEB_TAILSCALE_LOGIN: login },
    });
    server.stderr.pipe(process.stderr);
    const lines = createInterface({ input: server.stdout });
    const [line] = await once(lines, "line", { signal: AbortSignal.timeout(8000) });
    const desktop = line.match(/http:\/\/127\.0\.0\.1:\d+$/)[0];
    for (let tries = 0; ; tries++) {
      if ((await fetch(desktop + "/api/study/counts").then(response => response.json())).preparing === false) break;
      assert.ok(tries < 50, "Study queue failed to prepare");
      await delay(100);
    }
    await accessChecks(db);

    // A local transport translates the browser's loopback Origin and adds the
    // Serve headers. The outer deployment check uses the real Tailscale proxy.
    proxy = createServer((incoming, outgoing) => {
      const headers = { ...incoming.headers, ...serveHeaders };
      if (headers.origin === "http://" + incoming.headers.host) headers.origin = origin;
      const forward = request(upstream + incoming.url, { method: incoming.method, headers }, reply => {
        outgoing.writeHead(reply.statusCode, reply.headers);
        reply.pipe(outgoing);
      });
      forward.on("error", () => outgoing.destroy());
      incoming.pipe(forward);
    }).listen(0, "127.0.0.1");
    await once(proxy, "listening");
    await journey(db, desktop, "http://127.0.0.1:" + proxy.address().port, phone);
    console.log(`Private Lean ${phone ? "phone WebKit" : "desktop Chromium"} passed.`);
  } finally {
    proxy?.closeAllConnections();
    proxy?.close();
    if (server?.pid && server.exitCode === null && server.signalCode === null) {
      server.kill("SIGTERM");
      await once(server, "exit");
    }
    await rm(root, { recursive: true, force: true });
  }
}

(async () => { await run(false); await run(true); })().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
