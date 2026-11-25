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
  if (sql(db, "SELECT count(*) FROM sqlite_schema WHERE name='coverage_rank_input'") === "1") {
    sql(db, `INSERT INTO coverage_rank_input VALUES (${quoted(id)},'{}',0);`);
  }
}

async function withServer(binary, db, operation, env = process.env) {
  const socket = net.createServer();
  socket.listen(0, "127.0.0.1");
  await once(socket, "listening");
  const port = socket.address().port;
  await new Promise(resolve => socket.close(resolve));
  const origin = "http://127.0.0.1:" + port;
  const server = spawn(binary, ["serve", db, String(port), "--no-worker"], { stdio: ["ignore", "ignore", "inherit"], env });
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
    const draft = { report: "browser", gap: 0, pgn: originalPgn };
    const conflict = (await api("coverage/preview", draft)).conflicts[0];
    sql(db, `WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<2000)
      INSERT INTO openings(id,repertoire_id,name,color,route,start_at)
      SELECT printf('!fixture-%04d',i),${quoted(repertoire)},'Other line','white',
      ${quoted(JSON.stringify({ fen: start, moves: ["e2e4"] }))},0 FROM n;`);
    const limited = await api("coverage/preview", draft);
    assert.equal(limited.conflicts[0].saved.line, conflict.saved.line);
    assert.equal(limited.conflicts[0].transposition, true);
    assert.equal(limited.conflicts[0].saved_route_recorded, true);
    assert.equal(limited.conflicts[0].saved_route_limited, true);
    const unavailable = await api("coverage/preview", {
      ...draft, replacements: { [conflict.position]: "d2d4" },
    }, 422);
    assert.equal(unavailable.code, "replacement_preview_unavailable");
    assert.equal(unavailable.reason, "work_limit");
    sql(db, "DELETE FROM openings WHERE id GLOB '!fixture-*';");

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
    let replayed;
    let deliverReplay;
    const holdReplay = new Promise(resolve => { deliverReplay = resolve; });
    const replayReady = new Promise(resolve => { replayed = resolve; });
    await page.route("**/api/coverage/replace", async route => {
      const response = await route.fetch();
      replayed({ request: route.request().postDataJSON(), accepted: response.ok() });
      await holdReplay;
      await route.fulfill({ response });
    }, { times: 1 });
    await page.getByRole("button", { name: "Retry same save" }).click();
    try {
      assert.deepEqual(await replayReady, { request, accepted: true });
      await page.getByRole("button", { name: "Games", exact: true }).click();
      await page.getByRole("button", { name: "Openings", exact: true }).click();
      await page.getByRole("button", { name: "Retry same save" }).waitFor();
      deliverReplay();
      await page.getByText("Repertoire changes and continuation saved.", { exact: true }).waitFor();
      assert.equal(await page.getByRole("button", { name: "Retry same save" }).count(), 0);
    } finally {
      deliverReplay();
    }
    assert.equal(sql(db, "SELECT count(*) FROM gap_save_receipt;"), "1");
    assert.equal(sql(db, "SELECT count(*) FROM openings WHERE archived=1;"), "2");
    await page.close();
  });
}

async function saveReplyAcrossViews(browser, remount) {
  const db = path.join(folder, remount ? "save-remount.sqlite3" : "save-peer.sqlite3");
  await withServer(candidate, db, async (origin, api) => {
    const repertoire = await addOpening(api, "Save remount", ["e2e4", "e7e5", "g1f3"]);
    seedReport(db, "saved-report", repertoire, anchor, ["g1f3", "b8c6"]);
    const context = await browser.newContext();
    const page = await context.newPage();
    await context.addInitScript(key => {
      if (!localStorage.getItem(key)) localStorage.setItem(key, JSON.stringify({
        report: "saved-report", index: 0, pgn: "1. Nf3 Nc6 2. d4 *"
      }));
    }, journal);
    let saves = 0;
    let original;
    let scans = 0;
    let committed;
    let deliver;
    const heldReply = new Promise(resolve => { deliver = resolve; });
    const saved = new Promise(resolve => { committed = resolve; });
    await page.route("**/api/coverage/save", async route => {
      saves++;
      const request = route.request().postDataJSON();
      if (saves === 1) {
        original = request;
        await route.fulfill({ json: { command: request.command, lines: [null] } });
        return;
      }
      assert.deepEqual(request, original);
      const response = await route.fetch();
      committed(response.ok());
      await heldReply;
      await route.fulfill({ response });
    });
    await page.route("**/api/coverage", async route => {
      scans++;
      await route.abort();
    });
    try {
      await openGaps(page, origin);
      await page.getByRole("button", {name:"Preview", exact:true}).click();
      await page.getByRole("button", {name:"Save to repertoire", exact:true}).click();
      await page.getByRole("alert").waitFor();
      assert.equal(sql(db, "SELECT count(*) FROM gap_save_receipt"), "0");
      assert.deepEqual(await page.evaluate(key => JSON.parse(localStorage.getItem(key)), journal + ".save"), original);
      await page.getByRole("button", {name:"Retry same save"}).click();
      assert.equal(await saved, true);
      assert.equal(sql(db, "SELECT count(*) FROM gap_save_receipt"), "1");
      const retry = page.getByRole("button", {name:"Retry same save"});
      if (remount) {
        await page.getByRole("button", {name:"Games", exact:true}).click();
        await page.getByRole("button", {name:"Openings", exact:true}).click();
        await retry.waitFor();
        deliver();
      } else {
        const peer = await context.newPage();
        await openGaps(peer, origin);
        await peer.route("**/api/coverage", route => route.abort());
        await peer.getByRole("button", { name: "Retry same save" }).click();
        await peer.getByText("Continuation saved.", { exact: true }).waitFor();
      }
      await page.getByText("Continuation saved.", {exact:true}).waitFor({timeout:3000});
      assert.equal(await retry.count(), 0);
      assert.equal(await page.getByText("Working…", {exact:true}).count(), 0);
      assert.equal(scans, 0);
      await page.getByRole("button", {name:"Retry scan", exact:true}).click({timeout:3000});
      await page.getByRole("alert").waitFor();
      if (!remount) {
        const delivered = page.waitForResponse("**/api/coverage/save");
        deliver();
        await delivered;
        assert.equal(await page.getByText("Continuation saved.", {exact:true}).count(), 1);
        assert.equal(await page.getByRole("alert").count(), 1);
      }
      assert.equal(scans, 1);
      assert.equal(saves, 2);
      assert.equal(sql(db, "SELECT count(*) FROM gap_save_receipt"), "1");
    } finally {
      deliver();
      await context.close();
    }
  });
}

async function applyReplyAcrossViews(browser) {
  const db = path.join(folder, "apply-remount.sqlite3");
  await withServer(candidate, db, async (origin, api) => {
    const repertoire = await addOpening(api, "Apply remount", ["e2e4", "e7e5", "g1f3"]);
    seedReport(db, "apply-report", repertoire, anchor, ["g1f3", "b8c6"]);
    const context = await browser.newContext();
    await context.addInitScript(key => {
      if (!localStorage.getItem(key)) localStorage.setItem(key, JSON.stringify({
        report: "apply-report", index: 0, pgn: "1. Nf3 Nc6 2. d4 *"
      }));
    }, journal);
    const page = await context.newPage();
    const second = await context.newPage();
    let deliver;
    let committed;
    const held = new Promise(resolve => { deliver = resolve; });
    const ready = new Promise(resolve => { committed = resolve; });
    await page.route("**/api/coverage/apply-report/apply", async route => {
      const response = await route.fetch();
      committed(response.ok());
      await held;
      await route.fulfill({ response });
    }, { times: 1 });
    try {
      await openGaps(page, origin);
      await page.getByRole("button", { name: "Apply to opening order" }).click();
      assert.equal(await ready, true);
      await page.getByRole("button", { name: "Games", exact: true }).click();
      await page.getByRole("button", { name: "Openings", exact: true }).click();
      await page.getByRole("button", { name: "Retry applying order" }).waitFor();
      await openGaps(second, origin);
      // The second tab resolves the exact replay, then starts a newer command.
      await second.getByRole("button", { name: "Retry applying order" }).click();
      await second.getByText(/Applied opening order for 0 reached cards/).waitFor();
      const editor = second.getByRole("textbox", { name: "Paste PGN from Lichess" });
      await editor.fill("1. Nf3 Nc6 2. e4 *");
      let nextCommand;
      await second.route("**/api/coverage/apply-report/apply", async route => {
        nextCommand = route.request().postDataJSON();
        await route.fulfill({ json: { ...nextCommand, id: "apply-report" } });
      }, { times: 1 });
      await second.getByRole("button", { name: "Apply to opening order" }).click();
      await second.getByRole("alert").waitFor();
      const delivered = page.waitForResponse("**/api/coverage/apply-report/apply");
      deliver();
      await delivered;
      await page.getByRole("button", { name: "Retry applying order" }).waitFor();
      assert.equal(await editor.inputValue(), "1. Nf3 Nc6 2. e4 *");
      assert.deepEqual(await page.evaluate(key => JSON.parse(localStorage.getItem(key)), journal + ".apply"),
        { ...nextCommand, report: "apply-report" });
      assert.equal(sql(db, "SELECT count(*) FROM applied_forecast"), "1");
      await second.getByRole("button", { name: "Retry applying order" }).click();
      await page.getByText(/Applied opening order for 0 reached cards/).waitFor();
      assert.equal(await page.getByRole("button", { name: "Retry applying order" }).count(), 0);
      assert.equal(sql(db, "SELECT count(*) FROM applied_forecast"), "2");
    } finally {
      deliver();
      await context.close();
    }
  });
}

async function laterGapAcrossTabs(browser) {
  const db = path.join(folder, "later-gap.sqlite3");
  await withServer(candidate, db, async (origin, api) => {
    const repertoire = await addOpening(api, "Later page", ["e2e4", "e7e5", "g1f3"]);
    seedReport(db, "later-report", repertoire, anchor, ["g1f3", "b8c6"]);
    const first = Array.from({length: 30}, (_, index) => ({
      fen: anchor.replace(" 2 2", " " + (10 + index) + " 2"), moves: ["g1f3", "b8c6"], probability: 1,
    }));
    const later = [
      { fen: anchor, moves: ["g1f3", "b8c6"], probability: 0.43 },
      { fen: anchor.replace(" 2 2", " 3 2"), moves: ["e2e4", "e7e5"], probability: 0.42 },
    ];
    sql(db, `UPDATE coverage_view SET header=json_set(header,'$.total',32) WHERE report_id='later-report';
      UPDATE coverage_gap_page SET gaps=${quoted(JSON.stringify(first))} WHERE report_id='later-report';
      INSERT INTO coverage_gap_page VALUES ('later-report',1,${quoted(JSON.stringify(later))});`);
    const context = await browser.newContext();
    await context.addInitScript(key => {
      if (!localStorage.getItem(key)) localStorage.setItem(key, JSON.stringify({ report: "later-report", index: 0 }));
    }, journal);
    const observer = await context.newPage();
    const author = await context.newPage();
    try {
      await openGaps(observer, origin);
      await openGaps(author, origin);
      await author.getByRole("button", { name: "Show more gaps" }).click();
      await author.getByRole("button", { name: /^42%/ }).click();
      await observer.getByRole("link", { name: "Open line in Lichess" }).waitFor();
      await observer.waitForFunction(() => document.querySelector("a[href*='e4']"));
      await author.getByRole("button", { name: /^43%/ }).click();
      await observer.waitForFunction(() => document.querySelector("a[href*='Nf3']"));
      const editor = author.getByRole("textbox", { name: "Paste PGN from Lichess" });
      await editor.fill("1. Nf3 Nc6 2. d4 *");
      await author.getByRole("button", { name: "Preview", exact: true }).click();
      let pending;
      await author.route("**/api/coverage/save", async route => {
        pending = route.request().postDataJSON();
        await route.abort();
      });
      await author.getByRole("button", { name: "Save to repertoire", exact: true }).click();
      await author.getByRole("alert").waitFor();
      assert.equal(pending.draft.gap, 30);
      await observer.route("**/api/coverage/save", async route => {
        assert.deepEqual(route.request().postDataJSON(), pending);
        await route.continue();
      });
      await observer.route("**/api/coverage", route => route.abort());
      await observer.getByRole("button", { name: "Retry same save" }).click();
      await observer.getByText("Continuation saved.", { exact: true }).waitFor();
      await author.getByText("Continuation saved.", { exact: true }).waitFor();
      assert.equal(await author.getByRole("alert").count(), 0);
      assert.equal(await author.getByText(/Saving continuation/).count(), 0);
      assert.equal(sql(db, "SELECT count(*) FROM gap_save_receipt"), "1");

      seedReport(db, "new-report", repertoire, anchor, ["g1f3", "b8c6"]);
      let releasePage;
      let fetchedPage;
      const holdPage = new Promise(resolve => { releasePage = resolve; });
      const pageFetched = new Promise(resolve => { fetchedPage = resolve; });
      await observer.route("**/api/coverage/later-report/gaps?offset=30", async route => {
        const response = await route.fetch();
        fetchedPage();
        await holdPage;
        await route.fulfill({ response });
      }, { times: 1 });
      try {
        await observer.getByRole("button", { name: "Show more gaps" }).click();
        await pageFetched;
        await author.route("**/api/coverage", route => route.fulfill({ json: { id: "new-report" } }));
        const loaded = observer.waitForResponse("**/api/coverage/new-report/gaps");
        await author.getByRole("button", { name: "Scan repertoire" }).click();
        await loaded;
        const apply = observer.getByRole("button", { name: "Apply to opening order" });
        await observer.getByRole("button", { name: "Show more gaps" }).waitFor({ state: "hidden" });
        assert.equal(await apply.isEnabled(), true);
        const delivered = observer.waitForResponse("**/api/coverage/later-report/gaps?offset=30");
        releasePage();
        await delivered;
        // Wait for the handler's continuation, including the run finalizer.
        await observer.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
        assert.equal(await apply.isEnabled(), true);
        assert.equal(await observer.getByRole("button", { name: "Show more gaps" }).count(), 0);
      } finally {
        releasePage();
      }
    } finally {
      await context.close();
    }
  });
}

async function cancelledScans(browser) {
  const db = path.join(folder, "cancellation.sqlite3");
  const model = path.join(folder, "Library/Application Support/Tempo Lean/Maia3");
  const launcher = path.join(model, "venv/bin/python");
  const marker = path.join(model, "pid");
  fs.mkdirSync(path.dirname(launcher), { recursive: true });
  fs.writeFileSync(launcher, '#!/bin/sh\necho $$ > "$3/pid"\nexec /bin/sleep 60\n', { mode: 0o700 });

  await withServer(candidate, db, async (origin, api) => {
    const repertoire = await addOpening(api, "Cancellation", ["e2e4", "e7e5", "g1f3"]);
    seedReport(db, "saved-report", repertoire, anchor, ["g1f3", "b8c6"]);
    const page = await browser.newPage();
    await page.addInitScript(key => {
      if (!localStorage.getItem(key)) localStorage.setItem(key, JSON.stringify({
        report: "saved-report", index: 0, pgn: "1. Nf3 Nc6 2. d4 *"
      }));
    }, journal);
    async function leaveDuringModel() {
      await waitFor(() => fs.existsSync(marker) && Number(fs.readFileSync(marker, "utf8")) > 0);
      const pid = Number(fs.readFileSync(marker, "utf8"));
      await page.getByRole("button", {name:"Games",exact:true}).click();
      await waitFor(() => {
        try { process.kill(pid, 0); return false; }
        catch (error) { if (error.code === "ESRCH") return true; throw error; }
      });
      fs.unlinkSync(marker);
    }
    await openGaps(page, origin);
    await page.getByRole("button", { name: "Scan repertoire" }).click();
    await leaveDuringModel();
    await openGaps(page, origin);
    await page.getByRole("button", { name: "Preview", exact:true }).click();
    await page.getByRole("button", { name: "Save to repertoire", exact:true }).click();
    await leaveDuringModel();
    await openGaps(page, origin);
    await page.getByText("Continuation saved.", {exact:true}).waitFor();
    assert.equal(await page.getByRole("button", {name:"Retry same save"}).count(), 0);
    assert.equal(sql(db, "SELECT count(*) FROM gap_save_receipt"), "1");
    assert.equal(sql(db, "SELECT count(*) FROM coverage_report"), "1");
    await page.close();
  }, {...process.env, HOME:folder});
}

async function waitFor(predicate) {
  for (let attempt=0; attempt<250; attempt++) {
    if (predicate()) return;
    await new Promise(resolve => setTimeout(resolve, 20));
  }
  throw new Error("Timed out waiting for the disposable model process");
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
    await saveReplyAcrossViews(browser, true);
    await saveReplyAcrossViews(browser, false);
    await applyReplyAcrossViews(browser);
    await laterGapAcrossTabs(browser);
    await browserFlow(browser);
    await cancelledScans(browser);
    await previousCommands();
    console.log("Chromium conflict resolution, confirmed replacement retry, and installed predecessor command replay passed.");
  } finally {
    try { await browser?.close(); } finally { fs.rmSync(folder, { recursive: true, force: true }); }
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
