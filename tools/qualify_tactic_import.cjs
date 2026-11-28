// Run with a built UI and binary. NODE_PATH must include playwright-core.
const assert = require("node:assert/strict");
const { spawn } = require("node:child_process");
const { once } = require("node:events");
const { mkdtemp, readFile, rm } = require("node:fs/promises");
const { tmpdir } = require("node:os");
const path = require("node:path");
const { createInterface } = require("node:readline");
const { chromium } = require("playwright-core");

const url = "https://www.chess.com/puzzles/problem/2293026";
const fen = "r3k2r/3q1p2/8/p6p/P3pRn1/B1P5/3P3P/1N1QK3 w kq - 0 2";

async function qualify() {
  const directory = await mkdtemp(path.join(tmpdir(), "tempo-tactic-import-"));
  const binary = process.argv[2] ?? path.join(__dirname, "../target/debug/tempo");
  const server = spawn(binary, ["serve", path.join(directory, "fixture.sqlite3"), "0", "--no-worker"]);
  server.stderr.pipe(process.stderr);
  let browser;
  try {
    const lines = createInterface({ input: server.stdout });
    const [line] = await Promise.race([
      once(lines, "line", { signal: AbortSignal.timeout(5000) }),
      once(server, "exit").then(([code]) => { throw new Error(`Tempo exited with ${code}`); }),
    ]);
    const origin = line.match(/http:\/\/127\.0\.0\.1:\d+$/)[0];
    browser = await chromium.launch({ executablePath: process.env.TEMPO_QUALIFY_CHROME });
    const page = await browser.newPage();
    page.setDefaultTimeout(8000);
    const faults = [];
    const offHost = [];
    page.on("pageerror", (error) => faults.push(error.message));
    await page.route("**/*", (route) => {
      if (new URL(route.request().url()).origin === origin) return route.continue();
      offHost.push(route.request().url());
      return route.abort();
    });

    const pgn = await readFile(path.join(__dirname, "../tests/fixtures/chesscom-2293026.pgn"), "utf8");
    const linkFree = pgn.replace(/^\[Link .*\]\n/m, "");
    const editor = page.getByRole("textbox", { name: "Puzzle PGN", exact: true });
    const format = page.getByLabel("Import format");
    const setup = page.getByLabel("First move is setup");
    const add = page.getByRole("button", { name: "Add puzzle" });
    const backlink = page.getByRole("link", { name: "Open on Chess.com" });
    const importView = async () => {
      await page.getByRole("button", { name: "Tactics", exact: true }).click();
      await page.getByRole("button", { name: "Puzzle library" }).click();
    };
    const save = async (name) => {
      await page.getByLabel("Puzzle name").fill(name);
      const response = page.waitForResponse((reply) =>
        new URL(reply.url()).pathname === "/api/tactics" && reply.request().method() === "POST");
      await add.click();
      const reply = await response;
      assert.equal(reply.ok(), true, await reply.text());
      return reply.json();
    };
    const awaitFormat = (value) => page.locator(`select:has(option[value="${value}"]:checked)`).waitFor();

    await page.goto(origin);
    await importView();
    await editor.fill(pgn);
    await awaitFormat("chess_com_puzzle");
    assert.equal(await setup.isChecked(), true);
    const saved = await save("Chess.com export");
    assert.equal(saved.puzzle.route.fen, fen);
    assert.equal(saved.puzzle.route.moves[0], "f4e4");
    await page.getByText("Position 0 of 5", { exact: true }).waitFor();
    assert.equal(await backlink.getAttribute("href"), url);
    assert.equal(await page.getByLabel("Game moves").getByRole("button").first().textContent(), "Rxe4+");

    await page.getByLabel("Upload puzzle PGN").setInputFiles({
      name: "link-free.pgn", mimeType: "application/x-chess-pgn", buffer: Buffer.from(linkFree),
    });
    await page.getByLabel("Puzzle URL (optional)").fill(url);
    await awaitFormat("chess_com_puzzle");
    assert.equal(await setup.isChecked(), true);
    assert.equal((await save("URL field")).id, saved.id);

    await editor.fill(linkFree);
    await awaitFormat("generic_pgn");
    assert.equal(await setup.isChecked(), false);
    await format.selectOption("chess_com_puzzle");
    assert.equal(await setup.isChecked(), true);
    assert.equal((await save("Manual format")).id, saved.id);

    await editor.fill(
      `[SetUp "1"]\n[FEN "${fen}"]\n[Link "${url}"]\n\n2. Rxe4+ Kd8 3. Rd4 Re8+ 4. Kf1 *`);
    await awaitFormat("chess_com_puzzle");
    await setup.uncheck();
    assert.equal((await save("Pre-trimmed")).id, saved.id);

    await page.reload();
    await importView();
    await page.getByRole("button", { name: "Chess.com export", exact: true }).click();
    assert.equal(await backlink.getAttribute("href"), url);
    await page.getByText("Position 0 of 5", { exact: true }).waitFor();
    for (const other of ["https://www.chess.com/game/live/2293026", "https://www.chess.com.example/puzzles/problem/2293026"]) {
      await editor.fill(pgn.replace(url, other));
      await page.locator("button.primary:enabled").filter({ hasText: "Add puzzle" }).waitFor();
      assert.equal(await format.inputValue(), "generic_pgn");
      assert.equal(await setup.isChecked(), false);
    }
    await page.route("**/api/tactics/import-options", (route) => route.fulfill({
      status: 503, json: { error: "Format detection temporarily unavailable" },
    }), { times: 1 });
    await editor.fill(linkFree);
    await page.getByRole("alert").filter({ hasText: "Format detection temporarily unavailable" }).waitFor();
    await format.selectOption("chess_com_puzzle");
    assert.equal((await save("Manual with detection unavailable")).id, saved.id);
    assert.deepEqual(faults, []);
    assert.deepEqual(offHost, []);
    console.log("Packaged Puzzle library passed paste, upload, format and setup overrides, reload, and local-only requests.");
  } finally {
    await browser?.close();
    if (server.exitCode === null && server.signalCode === null && server.pid) {
      server.kill("SIGTERM");
      await once(server, "exit");
    }
    await rm(directory, { recursive: true, force: true });
  }
}

qualify().catch((error) => { console.error(error); process.exitCode = 1; });
