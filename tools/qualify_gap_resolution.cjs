// Usage: NODE_PATH=<playwright-core parent> node tools/qualify_gap_resolution.cjs <candidate> <installed predecessor>
const { chromium } = require("playwright-core");
const assert = require("node:assert/strict");
const { spawn, execFileSync } = require("node:child_process");
const { once } = require("node:events");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const net = require("node:net");

const [candidate, predecessor] = process.argv.slice(2).map(value => path.resolve(value));
const chrome = process.env.TEMPO_QUALIFY_CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const folder = fs.mkdtempSync(path.join(os.tmpdir(), "tgr-"));
const journal = "tempo-lean-gap-draft-v1";
const start = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
const anchor = "r1bqkbnr/pppppppp/2n5/8/8/5N2/PPPPPPPP/RNBQKB1R w KQkq - 2 2";
const originalPgn = "1. Nf3 Nc6 2. e4 e5 3. Nc3 Nf6 4. d4 exd4 5. Nxd4 *";
const twoConflicts = originalPgn.replace("2. e4 e5", "2. e4 e5 (2... e6 3. d4)");

function sql(db, command) {
  return execFileSync("sqlite3", [db, command], { encoding: "utf8" }).trim();
}

const quoted = value => "'" + String(value).replaceAll("'", "''") + "'";
function seedReport(db, id, repertoire, fen, moves) {
  const scan = JSON.stringify({ repertoire, color: "white", route: { fen: start, moves: [] }, through: 6 });
  const gaps = JSON.stringify([{ fen, moves, probability: 1 }]);
  const header = JSON.stringify({ model: "Maia-3", model_superseded: false, authored: 0, ended: 0, conflict: 0, unresolved: 0, total: 1 });
  const modelId = "maia3-5m-1500-history-v1";
  const hasModel = sql(db, "SELECT count(*) FROM pragma_table_info('coverage_view') WHERE name='model_id'") === "1";
  const report = JSON.stringify({ model: modelId, gaps: JSON.parse(gaps), reach: {}, paths: [], ended: 0, conflict: 0, unresolved: 0 });
  const projectedModel = hasModel ? "," + quoted(JSON.stringify(modelId)) : "";
  sql(db, `INSERT INTO coverage_report
    VALUES (${quoted(id)},${quoted(repertoire)}, (SELECT revision FROM coverage_revision WHERE repertoire=${quoted(repertoire)}),${quoted(scan)},${quoted(report)});
    INSERT INTO coverage_view VALUES (${quoted(id)},${quoted(header)},'{"cards":[],"paths":[]}'${projectedModel});
    INSERT INTO coverage_gap_page VALUES (${quoted(id)},0,${quoted(gaps)});`);
}

async function withServer(binary, db, operation) {
  const socket = net.createServer();
  socket.listen(0, "127.0.0.1");
  await once(socket, "listening");
  const port = socket.address().port;
  await new Promise(resolve => socket.close(resolve));
  const origin = "http://127.0.0.1:" + port;
  const server = spawn(binary, ["serve", db, String(port), "--no-worker"], { stdio: ["ignore", "ignore", "inherit"] });
  const api = async (endpoint, body, expected = 200) => {
    const response = await fetch(origin + "/api/" + endpoint, body === undefined ? {} : {
      method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body),
    });
    const data = await response.json();
    assert.equal(response.status, expected, JSON.stringify(data));
    return data;
  };
  try {
    for (let tries = 0; ; tries++) {
      if (await api("health").then(() => true, () => false)) break;
      if (tries === 149) throw new Error("Fixture server did not start");
      await new Promise(resolve => setTimeout(resolve, 20));
    }
    return await operation(origin, api);
  } finally {
    if (server.exitCode === null && server.signalCode === null) {
      const closed = once(server, "exit");
      server.kill();
      await closed;
    }
  }
}

async function openGaps(page, origin) {
  await page.goto(origin);
  await page.getByRole("button", { name: "Openings", exact: true }).click();
  await page.getByRole("button", { name: "Gaps", exact: true }).click();
  await page.getByRole("textbox", { name: "Paste PGN from Lichess" }).waitFor();
}

async function addOpening(api, name, moves, repertoire) {
  return (await api("openings", {
    name, color: "white", route: { fen: start, moves },
    ...(repertoire ? { destination: { kind: "existing", repertoire_id: repertoire } } : {}),
  })).id;
}

async function browserFlow(browser) {
  const db = path.join(folder, "browser.sqlite3");
  await withServer(candidate, db, async (origin, api) => {
    const line = ["e2e4", "e7e5", "g1f3", "b8c6", "b1c3", "g8f6", "f1b5"];
    const repertoire = await addOpening(api, "Main reply", [...line, "a7a6", "b5c6"]);
    await addOpening(api, "Keep legal tail", [...line, "h7h6", "a2a3"], repertoire);
    await addOpening(api, "Other conflict", ["e2e4", "e7e6", "g1f3", "b8c6", "b1c3"], repertoire);
    seedReport(db, "browser", repertoire, anchor, ["g1f3", "b8c6"]);
    const page = await browser.newPage({ viewport: { width: 820, height: 800 } });
    await page.addInitScript(({ key, pgn }) => {
      if (!localStorage.getItem(key)) localStorage.setItem(key, JSON.stringify({ report: "browser", index: 0, pgn }));
    }, { key: journal, pgn: twoConflicts });
    await openGaps(page, origin);

    const editor = page.getByRole("textbox", { name: "Paste PGN from Lichess" });
    const preview = page.getByRole("button", { name: "Preview", exact: true });
    const save = page.getByRole("button", { name: "Save to repertoire", exact: true });
    await preview.click();
    await page.getByRole("heading", { name: /move 4.*Same position by transposition/ }).waitFor();
    assert.equal(await page.locator(".gap-conflict").count(), 2);
    assert.equal(await save.isDisabled(), true);
    const recommended = page.getByRole("button", { name: /Use repertoire reply.*Bb5.*Recommended/ });
    assert.equal(await recommended.evaluate(button => getComputedStyle(button).backgroundColor), "rgb(33, 77, 54)");
    await recommended.focus();
    await page.keyboard.press("Enter");
    await page.getByRole("heading", { name: "These branches would lose later moves" }).waitFor();
    assert.equal(await editor.inputValue(), twoConflicts);
    await page.getByRole("button", { name: "Keep editing" }).click();
    assert.equal(await editor.inputValue(), twoConflicts);
    await recommended.click();
    await page.getByRole("button", { name: "Keep legal moves" }).click();
    await page.getByText("1 new reply · 1 conflict", { exact: true }).waitFor();
    assert.match(await editor.inputValue(), /4\. Bb5/);
    assert.equal(await save.isDisabled(), true);
    await editor.fill(originalPgn.replace("4. d4 exd4 5. Nxd4", "4. Bb5"));
    await preview.click();
    await page.getByText("1 new reply · No remaining conflicts", { exact: true }).waitFor();
    assert.equal(await save.isEnabled(), true);

    await editor.fill(originalPgn);
    assert.equal(await save.isDisabled(), true);
    await preview.click();
    let release;
    let received;
    const held = new Promise(resolve => { release = resolve; });
    const responseHeld = new Promise(resolve => { received = resolve; });
    await page.route("**/api/coverage/correct", async route => {
      const response = await route.fetch();
      received();
      await held;
      await route.fulfill({ response });
    }, { times: 1 });
    await page.getByRole("button", { name: /Use new reply in both/ }).click();
    await responseHeld;
    await page.getByRole("button", { name: "Games", exact: true }).click();
    const delivered = page.waitForResponse("**/api/coverage/correct");
    release();
    await delivered;
    await page.getByRole("button", { name: "Openings", exact: true }).click();
    assert.equal(await editor.inputValue(), originalPgn);
    assert.equal(await page.getByText("Staged repertoire changes", { exact: true }).count(), 0);
    await preview.click();
    await page.getByRole("button", { name: /Use new reply in both/ }).click();
    await page.getByRole("heading", { name: "Affected saved lines" }).waitFor();
    const details = page.getByRole("region", { name: "Saved line changes" });
    assert.match(await details.innerText(), /5\. Bxc6 becomes illegal/);
    assert.match(await details.innerText(), /5\. a3/);
    const confirm = page.getByRole("button", { name: "Confirm repertoire changes and save" });
    assert.equal(await confirm.count(), 0);
    await page.getByRole("button", { name: "Review repertoire changes" }).click();
    assert.equal(await confirm.isEnabled(), true);
    await editor.fill(originalPgn + " ");
    assert.equal(await confirm.count(), 0);
    await preview.click();
    await page.getByRole("button", { name: "Review repertoire changes" }).click();

    let request;
    await page.route("**/api/coverage/replace", async route => {
      request = route.request().postDataJSON();
      assert.ok((await route.fetch()).ok());
      await route.abort();
    }, { times: 1 });
    await page.route("**/api/coverage", route => route.abort());
    await confirm.click();
    await page.getByRole("alert").waitFor();
    assert.ok(request);
    await page.reload();
    await openGaps(page, origin);
    await page.getByRole("button", { name: "Retry same save" }).click();
    await page.getByText("Repertoire changes and continuation saved.", { exact: true }).waitFor();
    assert.equal(sql(db, "SELECT count(*) FROM gap_save_receipt;"), "1");
    assert.equal(sql(db, "SELECT count(*) FROM openings WHERE archived=1;"), "2");
    await page.close();
  });
}

async function previousCommands() {
  const db = path.join(folder, "predecessor.sqlite3");
  const commands = await withServer(predecessor, db, async (_origin, api) => {
    const repertoire = await addOpening(api, "Original", ["e2e4", "e7e5", "g1f3"]);
    const board = await api("study/next", {});
    assert.equal(board.kind, "opening");
    const expectedMove = JSON.parse(sql(db, "SELECT c.answers FROM cards c JOIN attempts a ON a.card_id=c.id WHERE a.id=(SELECT active FROM study);"))[0];
    const study = { attempt: board.attempt, dataset: board.dataset, played: expectedMove };
    const answer = await api("study/answer", study);
    const next = await api("study/next", { after: board.attempt, dataset: board.dataset });
    const nextMove = JSON.parse(sql(db, "SELECT c.answers FROM cards c JOIN attempts a ON a.card_id=c.id WHERE a.id=(SELECT active FROM study);"))[0];
    const pendingStudy = { attempt: next.attempt, dataset: next.dataset, played: nextMove };
    seedReport(db, "accepted", repertoire, anchor, ["g1f3", "b8c6"]);
    const draft = { report: "accepted", gap: 0, pgn: "1. Nf3 Nc6 2. d4 *" };
    const { command } = await api("coverage/preview", draft);
    const accepted = { draft, command };
    const receipt = await api("coverage/save", accepted);
    seedReport(db, "pending", repertoire, anchor, ["g1f3", "b8c6"]);
    const pendingDraft = { report: "pending", gap: 0, pgn: "1. Nf3 Nc6 2. d4 d5 3. Nc3 *" };
    const pending = { draft: pendingDraft, command: (await api("coverage/preview", pendingDraft)).command };
    return { accepted, receipt, pending, study, answer, pendingStudy };
  });
  execFileSync(candidate, ["migrate-games", db]);
  assert.equal(sql(db, "PRAGMA integrity_check;"), "ok");
  assert.equal(sql(db, "PRAGMA foreign_key_check;"), "");
  await withServer(candidate, db, async (_origin, api) => {
    assert.deepEqual(await api("coverage/save", commands.accepted), commands.receipt);
    assert.deepEqual(await api("study/answer", commands.study), commands.answer);
    const pendingAnswer = await api("study/answer", commands.pendingStudy);
    assert.equal(pendingAnswer.rating, "Good");
    assert.deepEqual(await api("study/answer", commands.pendingStudy), pendingAnswer);
    const accepted = await api("coverage/save", commands.pending);
    assert.deepEqual(await api("coverage/save", commands.pending), accepted);
    assert.equal(sql(db, "SELECT count(*) FROM gap_save_receipt;"), "2");
  });
}

(async () => {
  let browser;
  try {
    browser = await chromium.launch({ executablePath: chrome, headless: true });
    await browserFlow(browser);
    await previousCommands();
    console.log("Chromium conflict resolution, confirmed replacement retry, and installed predecessor command replay passed.");
  } finally {
    try { await browser?.close(); } finally { fs.rmSync(folder, { recursive: true, force: true }); }
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
