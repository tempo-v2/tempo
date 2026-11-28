// Run against a packaged candidate. Optional predecessor cases use a fresh
// SQLite backup of the supplied database; the source is opened read-only.
const assert = require("node:assert/strict");
const { chromium } = require("playwright-core");
const { spawn, execFileSync } = require("node:child_process");
const { once } = require("node:events");
const fs = require("node:fs");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");

const [candidate, predecessor, retained, worker, stockfish] = process.argv.slice(2);
assert.ok(candidate, "Provide CANDIDATE [PREDECESSOR RETAINED_DB [TACTIC_WORKER STOCKFISH]]");
const root = fs.mkdtempSync(path.join(os.tmpdir(), "tmr-"));
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
const sql = (db, query, readOnly = true) => execFileSync("sqlite3", [
  ...(readOnly ? ["-readonly"] : []), "-cmd", ".timeout 5000", db, query,
], { encoding: "utf8" }).trim();
const literal = value => "'" + value.replaceAll("'", "''") + "'";
const button = (page, name) => page.getByRole("button", { name, exact: true });
const study = (page, action) => page.getByRole("status").filter({ hasText: action }).waitFor();
const editPath = "openings/study-review";
const answerPattern = "**/api/study/answer";

async function port() {
  const socket = net.createServer().listen(0, "127.0.0.1");
  await once(socket, "listening");
  const port = socket.address().port;
  await new Promise(resolve => socket.close(resolve));
  return port;
}

async function server(binary, db, port, run, native = false) {
  const origin = "http://127.0.0.1:" + port;
  const service = spawn(binary, ["serve", db, String(port), "--no-worker"], { stdio: "ignore" });
  let engine;
  async function request(route, data) {
    const reply = await fetch(origin + "/api/" + route, data === undefined ? {} : {
      method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(data),
    });
    return { status: reply.status, body: await reply.json() };
  }
  async function api(route, data) {
    const { status, body } = await request(route, data);
    assert.equal(status, 200, JSON.stringify(body));
    return body;
  }
  try {
    for (let tries = 0; !await api("health").catch(() => null); tries++) {
      assert.ok(service.exitCode === null && tries < 300, "Fixture server failed to start");
      await pause(30);
    }
    if (native && worker) {
      engine = spawn(worker, ["watch", db, stockfish], { stdio: "ignore" });
    }
    return await run(origin, api, request);
  } finally {
    for (const child of [engine, service].filter(Boolean)) {
      if (child.exitCode === null && child.signalCode === null) {
        child.kill();
        await once(child, "exit");
      }
    }
  }
}

function focus(db, card) {
  sql(db, `BEGIN;
    INSERT INTO attempts(card_id) VALUES(${literal(card)});
    UPDATE study SET active=last_insert_rowid(),last_kind='opening';
    COMMIT;`, false);
}
function grade(db) {
  return Number(sql(db, "SELECT count(*) FROM attempts WHERE result IS NOT NULL"));
}

async function seed(db, api) {
  const { id } = await api("openings", { name: "Original", color: "white",
    route: { fen: "", moves: ["e2e4", "e7e5", "g1f3"] } });
  await api("openings", { name: "Other context", color: "white", start_at: 2,
    destination: { kind: "existing", repertoire_id: id },
    route: { fen: "", moves: ["g1f3", "g8f6", "e2e4"] } });
  const card = sql(db, `SELECT id FROM cards WHERE repertoire_id=${literal(id)}
    AND fen LIKE 'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR %'`);
  assert.ok(card);
  focus(db, card);
  return card;
}

async function play(page, uci) {
  const board = await page.locator("cg-board").boundingBox();
  for (const square of [uci.slice(0, 2), uci.slice(2, 4)]) {
    const file = square.charCodeAt(0) - 97;
    const rank = Number(square[1]) - 1;
    await page.mouse.click(board.x + (file + .5) * board.width / 8, board.y + (7.5 - rank) * board.height / 8);
  }
}

async function studyJourney(browser, { baseline, label, action, phone, native, remove = false }) {
  const db = path.join(root, `journey-${crypto.randomUUID().slice(0, 8)}.db`);
  fs.copyFileSync(baseline, db);
  sql(db, `UPDATE cards SET title=${literal(label)} WHERE id=(SELECT a.card_id FROM study s
    JOIN attempts a ON a.id=s.active)`, false);
  const page = await browser.newPage({ viewport: phone ? { width: 390, height: 844 } : { width: 1200, height: 850 } });
  try {
    return await server(candidate, db, await port(), async (origin, api, request) => {
      const cardId = sql(db, "SELECT a.card_id FROM study s JOIN attempts a ON a.id=s.active");
      const card = await api("study");
      const address = { dataset: card.dataset, attempt: card.attempt };
      assert.equal((await request(editPath, address)).status, 409);
      const commands = [];
      page.on("request", req => {
        if (req.method() === "POST") commands.push([new URL(req.url()).pathname.slice(5), req.postDataJSON()]);
      });
      if (remove) {
        await page.route("**/api/" + editPath + "/evidence", route =>
          route.request().postDataJSON().source === "similarity" ? route.abort() : route.continue());
      }
      await page.goto(origin);
      await button(page, "Review after answer").click();
      assert.deepEqual(commands, []);
      if (action === "reveal") await button(page, "Reveal answer").click();
      else await play(page, action === "good" ? "e2e4" : "d2d4");
      await page.getByRole("heading", { name: "Review repertoire move" }).waitFor();
      await study(page, action === "good" ? "Good" : "Again");
      await page.getByText("Saved reply e4", { exact: true }).waitFor();
      if (remove) {
        await button(page, "Retry similar saved positions").waitFor();
      } else {
        await page.getByText("Closest other saved position", { exact: false }).waitFor({ timeout: 7000 });
        await button(page, "Show closest saved position").click();
        await button(page, "Show Study position").click();
      }
      if (native && worker) {
        await page.getByText("Stockfish depth 10", { exact: false }).waitFor();
        assert.equal(await page.getByLabel("Compare a legal move").locator("option").count(), 20);
      } else {
        await button(page, "Retry Stockfish").waitFor();
      }
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
      await pause(900);
      assert.equal((await api("study")).attempt, card.attempt);
      assert.equal(commands.filter(([kind]) => kind === "study/next").length, 0);
      assert.equal(grade(db), 1);
      if (remove) {
        await button(page, "Preview removal").click();
        await page.getByRole("heading", { name: "Remove e4" }).waitFor();
        await button(page, "Confirm removal").click();
        await study(page, "Move removed");
        assert.equal(grade(db), 1);
        assert.equal(sql(db, `SELECT opening_retired FROM cards WHERE id=${literal(cardId)}`), "1");
      } else if (!native) {
        await button(page, "Close review").click();
        await button(page, "Review move").click();
        await page.getByText("Saved reply e4", { exact: true }).waitFor();
        await button(page, "Close review").click();
        await page.route("**/api/study/next", route => route.abort(), { times: 1 });
        await button(page, "Next card").click();
        await button(page, "Retry").click();
        assert.deepEqual(commands.filter(([kind]) => ["study/answer", "study/next"].includes(kind)), [
          ["study/answer", { ...address, played: action === "reveal" ? null : action === "good" ? "e2e4" : "d2d4" }],
          ["study/next", { dataset: address.dataset, after: address.attempt }],
          ["study/next", { dataset: address.dataset, after: address.attempt }],
        ]);
        assert.equal(grade(db), 1);
      } else {
        await page.getByLabel("Compare a legal move").selectOption("d2d4");
        await button(page, "Preview replacement").click();
        await page.getByRole("heading", { name: "Replace e4 with d4" }).waitFor();
        const results = sql(db, "SELECT result FROM attempts WHERE result IS NOT NULL");
        const learning = sql(db, "SELECT * FROM learning ORDER BY card_id");
        let sent;
        await page.route("**/api/" + editPath + "/apply", async route => {
          sent = route.request().postDataJSON();
          const reply = await route.fetch();
          assert.equal(reply.status(), 200, await reply.text());
          await route.abort();
        }, { times: 1 });
        await button(page, "Confirm replacement").click();
        await button(page, "Retry same repertoire save").waitFor();
        assert.ok(sent?.command);
        const other = await page.context().newPage();
        await other.goto(origin);
        const replay = other.waitForRequest(req => req.url().endsWith("/apply"));
        await button(other, "Retry same repertoire save").click();
        assert.deepEqual((await replay).postDataJSON(), sent);
        await study(other, "Replacement saved");
        await study(page, "Replacement saved");
        assert.equal(await page.locator(".study-panel [role=alert]").count(), 0);
        await other.close();
        await page.reload();
        await study(page, "Replacement saved");
        assert.equal(sql(db, "SELECT count(*) FROM gap_save_receipt"), "1");
        assert.equal(sql(db, "SELECT result FROM attempts WHERE result IS NOT NULL"), results);
        assert.equal(sql(db, "SELECT * FROM learning ORDER BY card_id"), learning);
        assert.equal(sql(db, `SELECT opening_retired FROM cards WHERE id=${literal(cardId)}`), "1");
        assert.equal(sql(db, "SELECT count(*) FROM active_cards WHERE answers='[\"d2d4\"]'"), "1");
      }
      return {
        commands: commands.filter(([kind]) => ["study/answer", "study/next"].includes(kind)),
        effects: sql(db, `SELECT id,card_id,result FROM attempts WHERE result IS NOT NULL;
          SELECT card_id,stability,difficulty,due_at-last_at FROM learning;`),
      };
    }, native);
  } finally { await page.context().close(); }
}

async function unmarkedAndNoRoute(browser, baseline) {
  const db = path.join(root, "unmarked.db");
  fs.copyFileSync(baseline, db);
  const page = await browser.newPage();
  try {
    await server(candidate, db, await port(), async (origin, api) => {
      const first = await api("study");
      await page.goto(origin);
      await button(page, "Review after answer").waitFor();
      const automatic = page.waitForRequest(req => req.url().endsWith("/api/study/next"));
      await play(page, "e2e4");
      assert.deepEqual((await automatic).postDataJSON(), { dataset: first.dataset, after: first.attempt });
      assert.equal(await page.getByRole("heading", { name: "Review repertoire move" }).count(), 0);
      assert.equal(grade(db), 1);
      const rootCard = sql(db, "SELECT card_id FROM attempts WHERE result IS NOT NULL");
      focus(db, rootCard);
      // Simulate a retained card whose repertoire no longer has an authored route.
      sql(db, "UPDATE openings SET archived=1", false);
      await page.reload();
      await button(page, "Reveal answer").click();
      await button(page, "Review move").click();
      await page.getByText("Study answer e4", { exact: true }).waitFor();
      assert.equal(await page.getByText("Saved reply e4", { exact: true }).count(), 0);
      assert.equal(await button(page, "Preview removal").isEnabled(), false);
      const { id } = await api("tactics", { name: "Tactic fixture", pgn: "1. a3 a6 1-0" });
      focus(db, id);
      await page.reload();
      await button(page, "Reveal answer").waitFor();
      assert.equal(await button(page, "Review after answer").count(), 0);
    });
  } finally { await page.context().close(); }
}

async function predecessorAnswer(browser, accepted) {
  const db = path.join(root, `prev-${accepted}.db`);
  execFileSync("sqlite3", ["-readonly", retained, ".backup " + db]);
  const card = sql(db, "SELECT id FROM active_cards WHERE kind='opening' LIMIT 1");
  focus(db, card);
  const p = await port();
  const page = await browser.newPage();
  const before = grade(db);
  let original;
  try {
    await server(predecessor, db, p, async origin => {
      await page.route(answerPattern, async route => {
        original = route.request().postDataJSON();
        if (accepted) assert.equal((await route.fetch()).status(), 200);
        await route.abort();
      }, { times: 1 });
      await page.goto(origin);
      const skip = button(page, "Skip to your move");
      await button(page, "Reveal answer").or(skip).first().waitFor();
      if (await skip.isVisible()) await skip.click();
      await button(page, "Reveal answer").click();
      await button(page, "Retry").waitFor();
      assert.equal(grade(db), before + Number(accepted));
    });
    await server(candidate, db, p, async () => {
      const replay = page.waitForRequest(req => req.url().endsWith("/api/study/answer"));
      await page.reload();
      assert.deepEqual((await replay).postDataJSON(), original);
      await button(page, "Review move").waitFor();
      assert.equal(grade(db), before + 1);
    });
  } finally { await page.context().close(); }
}

(async () => {
  const browser = await chromium.launch({ executablePath: process.env.TEMPO_QUALIFY_CHROME });
  try {
    const baseline = path.join(root, "baseline.db");
    await server(candidate, baseline, await port(), (_, api) => seed(baseline, api));
    await studyJourney(browser, { baseline, label: "Saved line", action: "good", native: true });
    const results = [];
    for (const label of ["Original", "Renamed", ""]) {
      results.push(await studyJourney(browser, { baseline, label, action: "reveal", phone: label === "" }));
    }
    // Cloned dataset, attempt and repertoire identities make the title-only comparison exact.
    assert.deepEqual(results.slice(1), [results[0], results[0]]);
    await studyJourney(browser, { baseline, label: "Wrong answer", action: "wrong", phone: true });
    await studyJourney(browser, { baseline, label: "Manual removal", action: "reveal", remove: true });
    await unmarkedAndNoRoute(browser, baseline);
    if (predecessor && retained) {
      for (const accepted of [true, false]) await predecessorAnswer(browser, accepted);
    }
    console.log("Packaged Study review, evidence, exact edit replay, Next-only retry and title invariance passed.");
  } finally {
    await browser.close();
    fs.rmSync(root, { recursive: true, force: true });
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
