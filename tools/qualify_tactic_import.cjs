// Run with a built UI and binary. NODE_PATH must include playwright-core.
// The ID journey uses one live request to the public Lichess puzzle endpoint.
const assert = require("node:assert/strict");
const { execFileSync, spawn } = require("node:child_process");
const { once } = require("node:events");
const { mkdtemp, readFile, rm, writeFile } = require("node:fs/promises");
const { tmpdir } = require("node:os");
const path = require("node:path");
const { createInterface } = require("node:readline");
const { setTimeout: delay } = require("node:timers/promises");
const { chromium } = require("playwright-core");

const chesscom = "https://www.chess.com/puzzles/problem/2293026";
const fen = "r3k2r/3q1p2/8/p6p/P3pRn1/B1P5/3P3P/1N1QK3 w kq - 0 2";
const captureRoute = "**/api/tactics/capture";

async function qualify() {
  const directory = await mkdtemp(path.join(tmpdir(), "tempo-capture-"));
  const binary = process.argv[2] ?? path.join(__dirname, "../target/debug/tempo");
  const db = path.join(directory, "fixture.sqlite3");
  const seed = path.join(directory, "seed.json");
  let server;
  let browser;
  try {
    await writeFile(seed, JSON.stringify({ pgn: "1. e4 e5 *", name: "Existing attempt" }));
    execFileSync(binary, ["import-content", db, "tactic", seed]);
    server = spawn(binary, ["serve", db, "0", "--no-worker"]);
    server.stderr.pipe(process.stderr);
    const lines = createInterface({ input: server.stdout });
    const [line] = await Promise.race([
      once(lines, "line", { signal: AbortSignal.timeout(5000) }),
      once(server, "exit").then(([code]) => { throw new Error(`Tempo exited with ${code}`); }),
    ]);
    const origin = line.match(/http:\/\/127\.0\.0\.1:\d+$/)[0];
    const api = async (endpoint, data) => {
      const response = await fetch(`${origin}/api/${endpoint}`, data === undefined ? {} : {
        method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(data),
      });
      return response.json();
    };
    let issued;
    for (let retries = 0; retries < 40; retries++) {
      issued = await api("study/next", { after: null });
      if (issued?.attempt) break;
      await delay(100);
    }
    assert.ok(issued?.attempt);
    const revealed = await api("study/answer", { attempt: issued.attempt, dataset: issued.dataset, played: null });
    assert.equal(revealed.rating, "Again");

    browser = await chromium.launch({ executablePath: process.env.TEMPO_QUALIFY_CHROME });
    const page = await browser.newPage();
    page.setDefaultTimeout(14000);
    const faults = [];
    const offHost = [];
    let nextCalls = 0;
    page.on("pageerror", (error) => faults.push(error.message));
    page.on("request", (request) => { if (new URL(request.url()).pathname === "/api/study/next") nextCalls++; });
    await page.route("**/*", (route) => {
      if (new URL(route.request().url()).origin === origin) return route.continue();
      offHost.push(route.request().url());
      return route.abort();
    });

    const fixture = name => readFile(path.join(__dirname, "../tests/fixtures", name), "utf8");
    const pgn = await fixture("chesscom-2293026.pgn");
    const chapter = await fixture("lichess-capture-chapter.pgn");
    const linkFree = pgn.replace(/^\[Link .*\]\n/m, "");
    const dialog = page.getByRole("dialog", { name: "Capture puzzle" });
    const editor = dialog.getByRole("textbox", { name: "Puzzle PGN or Lichess puzzle ID" });
    const format = dialog.getByLabel("Import format");
    const setup = dialog.getByLabel("First move is setup");
    const primary = dialog.getByRole("button", { name: "Save puzzle" });
    const headerCapture = page.locator("header").getByRole("button", { name: "Capture", exact: true });
    const expectReady = () => dialog.locator("textarea:focus").waitFor();
    const close = () => dialog.getByRole("button", { name: "Close" }).click();
    const importOptions = async () => {
      if (!await dialog.locator("details").evaluate(element => element.open)) {
        await dialog.getByText("Import options", { exact: true }).click();
      }
    };
    const expectFormat = name => dialog.locator(`select:has(option[value="${name}"]:checked)`).waitFor();
    const assertCleared = async () => {
      await expectReady();
      await dialog.getByText("Saved", { exact: true }).waitFor();
      assert.equal(await editor.inputValue(), "");
    };
    const save = async (button = primary) => {
      const response = page.waitForResponse(reply => /\/api\/tactics\/capture(?:\/lichess)?$/.test(reply.url()));
      await button.click();
      const reply = await response;
      assert.equal(reply.ok(), true, await reply.text());
      const result = await reply.json();
      await assertCleared();
      return result;
    };

    await page.goto(origin);
    await page.getByRole("button", { name: "Next card" }).waitFor();
    await headerCapture.click();
    await expectReady();
    await editor.fill(pgn);
    await importOptions();
    await expectFormat("chess_com_puzzle");
    assert.equal(await setup.isChecked(), true);

    let outgoing;
    let allowReply;
    let accepted;
    let wroteReceipt;
    const wrote = new Promise(resolve => { wroteReceipt = resolve; });
    await page.route(captureRoute, async route => {
      outgoing = route.request().postDataJSON();
      const response = await route.fetch();
      assert.equal(response.ok(), true, await response.text());
      accepted = await response.json();
      wroteReceipt();
      await new Promise(done => { allowReply = done; });
      await route.abort();
    }, { times: 1 });
    await primary.click();
    await wrote;
    await page.keyboard.press("Space");
    assert.equal(nextCalls, 0, "Space during Capture must not advance Study");
    assert.equal((await api("study")).attempt, issued.attempt);
    allowReply();
    const retry = dialog.getByRole("button", { name: "Retry save" });
    await retry.waitFor();
    assert.equal(await editor.inputValue(), pgn);
    await close();
    await headerCapture.click();
    const delivery = page.waitForRequest(request => request.url().endsWith("/api/tactics/capture"));
    const replay = await save(retry);
    assert.deepEqual((await delivery).postDataJSON(), outgoing);
    assert.deepEqual(replay, accepted);
    assert.equal(replay.puzzle.route.fen, fen);
    assert.equal(replay.puzzle.route.moves[0], "f4e4");

    await dialog.getByLabel("Upload puzzle PGN").setInputFiles({
      name: "chapter.pgn", mimeType: "application/x-chess-pgn", buffer: Buffer.from(chapter),
    });
    assert.equal((await save()).puzzle.route.moves[0], "e6f6");

    await editor.fill("PSjmf");
    const lichess = await save();
    assert.deepEqual(lichess.puzzle.route.moves, ["g8g7", "d5e5", "f6e5"]);
    assert.equal(lichess.puzzle.source_url, "https://lichess.org/training/PSjmf");

    await page.route("**/api/tactics/capture/lichess", route => route.abort(), { times: 1 });
    await editor.fill("00008");
    await primary.click();
    await retry.waitFor();
    await page.route("**/api/tactics/capture/lichess", route => route.fulfill({
      status: 502, json: { code: "capture_lookup_failed", error: "Fixture lookup failure" },
    }), { times: 1 });
    await retry.click();
    await dialog.getByRole("alert").getByText("Fixture lookup failure").waitFor();
    assert.equal(await editor.inputValue(), "00008");
    assert.equal(await editor.isEditable(), true);

    await editor.fill(linkFree);
    await importOptions();
    await format.selectOption("chess_com_puzzle");
    assert.equal((await save()).id, replay.id);

    const pretrimmed = `[FEN "${fen}"]\n[Link "${chesscom}"]\n\n2. Rxe4+ Kd8 3. Rd4 Re8+ 4. Kf1 *`;
    await editor.fill(pretrimmed);
    await expectFormat("chess_com_puzzle");
    await setup.uncheck();
    assert.equal((await save()).id, replay.id);
    await close();
    await page.locator("header button:focus").filter({ hasText: "Capture" }).waitFor();
    assert.equal((await api("study")).attempt, issued.attempt);

    await page.reload();
    await page.setViewportSize({ width: 390, height: 844 });
    await headerCapture.click();
    await expectReady();
    assert.ok((await dialog.boundingBox()).width < 390);
    await page.keyboard.press("Escape");
    await dialog.waitFor({ state: "hidden" });
    await page.getByRole("button", { name: "Tactics", exact: true }).click();
    await page.getByRole("button", { name: "Puzzle library" }).click();
    await page.getByRole("button", { name: "Chess.com puzzle 2293026", exact: true }).click();
    assert.equal(await page.getByRole("link", { name: "Open on Chess.com" }).getAttribute("href"), chesscom);
    await page.getByRole("button", { name: "Lichess puzzle PSjmf", exact: true }).click();
    assert.equal(await page.getByRole("link", { name: "Open on Lichess" }).getAttribute("href"), lichess.puzzle.source_url);
    await page.locator("main").getByRole("button", { name: "Capture", exact: true }).click();
    await expectReady();
    await close();

    assert.deepEqual(faults, []);
    assert.deepEqual(offHost, []);
    console.log("Packaged Capture passed all three sources, same-command retry, Study isolation, and desktop/phone access.");
  } finally {
    await browser?.close();
    if (server?.pid && server.exitCode === null && server.signalCode === null) {
      server.kill("SIGTERM");
      await once(server, "exit");
    }
    await rm(directory, { recursive: true, force: true });
  }
}

qualify().catch((error) => { console.error(error); process.exitCode = 1; });
