// Use only disposable copies. Usage is documented in README.md.
const assert = require("node:assert/strict");
const { execFileSync, spawn } = require("node:child_process");
const { once } = require("node:events");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { chromium } = require("playwright-core");

const [candidate, predecessor, snapshot] = process.argv.slice(2).map(value => path.resolve(value));
assert(candidate && Boolean(predecessor) === Boolean(snapshot), "Supply CANDIDATE [PREDECESSOR SNAPSHOT]");
const root = fs.mkdtempSync(path.join(os.tmpdir(), "tempo-feedback-"));
const start = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
const blackStart = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 27";
const whitePgn = '[White "WhiteFeedback"]\n[Black "Guest"]\n\n1. e4 e5 2. Nf3 Nc6 3. Bb5 a6 1/2-1/2';
const blackPgn = '[White "Guest"]\n[Black "BlackFeedback"]\n[SetUp "1"]\n' +
  '[FEN "' + blackStart + '"]\n\n27... e5 28. Nf3 Nc6 1/2-1/2';

const quote = value => "'" + String(value).replaceAll("'", "''") + "'";
function sql(db, statement) {
  return execFileSync("sqlite3", ["-cmd", ".timeout 3000", db, statement], { encoding: "utf8" }).trim();
}
function jobStatus(db, id) {
  return sql(db, "SELECT status FROM jobs WHERE id=" + Number(id));
}
async function waitFor(check, description) {
  const end = Date.now() + 25000;
  do {
    const result = await check();
    if (result) return result;
    await new Promise(resolve => setTimeout(resolve, 30));
  } while (Date.now() < end);
  throw new Error("Timed out waiting for " + description);
}
async function serve(binary, db, run, worker = true) {
  const service = spawn(binary, ["serve", db, "0", ...(worker ? [] : ["--no-worker"])],
    { stdio: ["ignore", "pipe", "inherit"] });
  let address = "";
  service.stdout.on("data", chunk => { address += chunk; });
  try {
    const origin = await waitFor(() => {
      assert.equal(service.exitCode, null, "fixture service exited");
      return address.match(/http:\/\/127\.0\.0\.1:\d+/)?.[0];
    }, "candidate listener");
    const api = async (endpoint, body) => {
      for (let attempt = 0; ; attempt++) {
        const reply = await fetch(origin + "/api/" + endpoint, {
          signal: AbortSignal.timeout(6000),
          ...(body === undefined ? {} : {
            method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body),
          }),
        });
        const data = await reply.json();
        if (body === undefined && data?.code === "view_unavailable" && attempt < 10) {
          await new Promise(resolve => setTimeout(resolve, 50));
          continue;
        }
        assert.equal(reply.ok, true, endpoint + " " + JSON.stringify(data));
        return data;
      }
    };
    await run({ origin, api });
  } finally {
    if (service.exitCode === null && service.signalCode === null) {
      const exited = once(service, "exit");
      service.kill();
      await exited;
    }
  }
}

const choicesPath = game => "games/archive/" + encodeURIComponent(game) + "/repertoires";
const feedbackPath = (game, book) => "games/archive/" + encodeURIComponent(game) + "/feedback/" + encodeURIComponent(book);
function addLine(api, name, color, moves, { fen = start, start_at = 0 } = {}) {
  return api("openings", { name, color, start_at, route: { fen, moves } }).then(result => result.id);
}
async function importGame(api, db, username, pgn) {
  const { id } = await api("games/uploads", { username, pgn });
  await waitFor(() => jobStatus(db, id) === "done", "PGN acceptance");
}
async function drained(db) {
  await waitFor(() => sql(db, "SELECT count(*) FROM jobs WHERE status IN ('queued','running')") === "0",
    "background turns");
}
async function enterGames(page, origin) {
  await page.goto(origin);
  await page.getByRole("button", { name: "Games", exact: true }).click();
}
function panel(page) {
  return page.getByRole("region", { name: "Repertoire feedback" });
}
function chooseGame(page, label) {
  return page.getByRole("button", { name: label }).first().click();
}
function chooseBook(page, value) {
  return panel(page).getByRole("combobox", { name: "Repertoire", exact: true }).selectOption(value);
}
function count(page, text) {
  return panel(page).getByText(text, { exact: true }).waitFor();
}
function studyState(db) {
  return ["learning", "attempts", "study", "prepared_queue", "prepared_queue_state", "deck", "repeat_obligation"]
    .map(table => sql(db, "SELECT * FROM " + table + " ORDER BY rowid"));
}

async function holdFeedbackRead(page, pathname) {
  let release, accepted;
  const held = new Promise(resolve => { release = resolve; });
  const intercepted = new Promise(resolve => { accepted = resolve; });
  await page.route("**/api/" + pathname + "*", async route => {
    const reply = await route.fetch();
    accepted();
    await held;
    await route.fulfill({ response: reply }).catch(() => {});
  }, { times: 1 });
  await intercepted;
  return release;
}

async function fixtureFlow(browser) {
  const db = path.join(root, "fixtures.sqlite3");
  let whiteGame, blackGame, italian, ruy, empty, black;
  await serve(candidate, db, async ({ api }) => {
    await importGame(api, db, "WhiteFeedback", whitePgn);
    await importGame(api, db, "BlackFeedback", blackPgn);
    await drained(db);
    const archive = await api("games");
    whiteGame = archive.find(row => row.learner_color === "white").id;
    blackGame = archive.find(row => row.learner_color === "black").id;
    italian = await addLine(api, "Feedback Italian", "white", ["e2e4", "e7e5", "f1c4"]);
    ruy = await addLine(api, "Feedback Ruy", "white", ["e2e4", "e7e5", "g1f3"]);
    empty = await addLine(api, "Feedback unassessed", "white", ["g1f3", "d7d5", "g2g3"], { start_at: 2 });
    black = await addLine(api, "Feedback Black", "black", ["c7c5"], { fen: blackStart });
  });

  const page = await browser.newPage({ viewport: { width: 1130, height: 880 } });
  page.on("pageerror", error => console.error("Browser error", error.message));
  const button = (name) => panel(page).getByRole("button", { name, exact: true });
  await serve(candidate, db, async ({ origin, api }) => {
    await enterGames(page, origin);
    await chooseGame(page, /WhiteFeedback vs Guest/);
    const select = panel(page).getByRole("combobox", { name: "Repertoire", exact: true });
    await select.waitFor({ timeout: 4000 }).catch(async error => {
      console.error(await page.locator("main").innerText());
      throw error;
    });
    assert.equal(await select.inputValue(), "", "multiple choices require explicit selection");
    assert.equal(await select.locator("option").count(), 4);
    await chooseBook(page, italian);
    await button("Compare game").click();
    await count(page, "Comparison queued.");
    await page.getByRole("button", { name: "Next move", exact: true }).click();
    await page.getByText("Position 1 of 6", { exact: true }).waitFor();
    const addressed = await api(feedbackPath(whiteGame, italian));
    assert.equal(addressed.generation, 1);
    assert.equal(addressed.report, null);
    assert.equal(sql(db, "SELECT count(*) FROM jobs WHERE operation='scan'"), "0");

    await chooseGame(page, /Guest vs BlackFeedback/);
    await button("Compare game").waitFor();
    assert.equal(await select.inputValue(), black, "only applicable repertoire defaults");
    await button("Compare game").click();
    await count(page, "Comparison queued.");
  }, false);

  await serve(candidate, db, async ({ origin, api }) => {
    await drained(db);
    await enterGames(page, origin);
    await chooseGame(page, /WhiteFeedback vs Guest/);
    await chooseBook(page, italian);
    await count(page, "1 of 2 repertoire decisions followed — 50.0%");
    await count(page, "1 mismatch. 1 unassessed learner move.");
    assert.equal(await panel(page).getByRole("list", { name: "Repertoire decisions" }).getByRole("listitem").count(), 1);
    await button("First repertoire mismatch").click();
    await page.getByText("Position 2 of 6", { exact: true }).waitFor();
    assert.match(await panel(page).locator(".feedback-position").innerText(), /Played Nf3 · Repertoire Bc4/);
    await button("All assessed").click();
    assert.equal(await panel(page).getByRole("listitem").count(), 2);
    await button("Unassessed").click();
    await panel(page).getByRole("button", { name: "View position at 3. Bb5", exact: true }).click();
    await page.getByText("Position 4 of 6", { exact: true }).waitFor();
    assert.match(await panel(page).locator(".feedback-position").innerText(), /No repertoire reply at this position/);

    const revealOldBook = await holdFeedbackRead(page, feedbackPath(whiteGame, italian));
    await chooseBook(page, empty);
    await button("Compare game").waitFor();
    revealOldBook();
    await button("Compare game").click();
    await count(page, "No comparable repertoire decisions.");
    assert.doesNotMatch(await panel(page).innerText(), /\d+\.\d%/);

    await chooseGame(page, /Guest vs BlackFeedback/);
    await count(page, "0 of 1 repertoire decisions followed — 0.0%");
    await page.getByRole("button", { name: "Next move", exact: true }).click();
    await page.getByText("Position 1 of 3", { exact: true }).waitFor();
    await button("First repertoire mismatch").click();
    await page.getByText("Position 0 of 3", { exact: true }).waitFor();
    assert.match(await panel(page).locator(".feedback-position").innerText(), /27\.\.\. Played e5 · Repertoire c5/);
    assert.equal((await api(choicesPath(blackGame))).choices.length, 1);
    const revealOldGame = await holdFeedbackRead(page, feedbackPath(blackGame, black));
    await chooseGame(page, /WhiteFeedback vs Guest/);
    await chooseBook(page, italian);
    await count(page, "1 of 2 repertoire decisions followed — 50.0%");
    revealOldGame();
    await button("First repertoire mismatch").click();
    await page.getByText("Position 2 of 6", { exact: true }).waitFor();
    await page.getByRole("button", { name: "Add opening line", exact: true }).click();
    await page.getByRole("heading", { name: "Openings", exact: true }).waitFor();

    await page.getByRole("button", { name: "Games", exact: true }).click();
    await chooseGame(page, /WhiteFeedback vs Guest/);
    await chooseBook(page, italian);
    const previous = await api(feedbackPath(whiteGame, italian));
    sql(db, `UPDATE cards SET answers='["g1f3"]', answer_version=(SELECT answer_version FROM cards
      WHERE repertoire_id=${quote(ruy)} AND answers='["g1f3"]') WHERE repertoire_id=${quote(italian)}
      AND answers='["f1c4"]';
      CREATE TRIGGER fixture_report_failure BEFORE UPDATE OF report ON repertoire_feedback
      WHEN NEW.repertoire_id=${quote(italian)} BEGIN SELECT RAISE(ABORT,'fixture comparison failed'); END;`);
    await button("Refresh comparison").click();
    await button("Retry comparison").waitFor();
    await count(page, "1 of 2 repertoire decisions followed — 50.0%");
    await button("First repertoire mismatch").click();
    assert.match(await panel(page).locator(".feedback-position").innerText(), /Repertoire Bc4/);
    sql(db, "DROP TRIGGER fixture_report_failure");
    await button("Retry comparison").click();
    await count(page, "2 of 2 repertoire decisions followed — 100.0%");
    assert.equal((await api(feedbackPath(whiteGame, italian))).generation, previous.generation + 2);

    const before = studyState(db);
    await importGame(api, db, "AutoFeedback", whitePgn.replace("WhiteFeedback", "AutoFeedback"));
    await drained(db);
    const auto = (await api("games")).find(row => row.white === "AutoFeedback").id;
    for (const book of [italian, ruy, empty]) assert((await api(feedbackPath(auto, book))).current);
    const backfill = await api("games/jobs", { provider: "archive", operation: "scan" });
    await waitFor(() => jobStatus(db, backfill.id) === "done", "archive backfill");
    assert((await api(feedbackPath(whiteGame, ruy))).current);
    const counts = sql(db, "SELECT count(*), sum(generation) FROM repertoire_feedback");
    const repeated = await api("games/jobs", { provider: "archive", operation: "scan" });
    await waitFor(() => jobStatus(db, repeated.id) === "done", "repeated backfill");
    assert.equal(sql(db, "SELECT count(*), sum(generation) FROM repertoire_feedback"), counts);
    assert.deepEqual(studyState(db), before, "feedback changed Study storage");
    await chooseGame(page, /AutoFeedback vs Guest/);
    await count(page, "2 of 2 repertoire decisions followed — 100.0%");
    assert.equal(await panel(page).getByRole("combobox", { name: "Repertoire" }).inputValue(), italian);
  });
  await page.close();
  console.log("Packaged Games passed individual comparison, replay, state and race checks, import, backfill, and Study storage isolation.");
}

async function predecessorFlow(browser) {
  const db = path.join(root, "retained.sqlite3");
  fs.copyFileSync(snapshot, db);
  assert.equal(sql(db, "PRAGMA user_version"), "18");
  let selected, book, legacyJob, legacyPly, asset;
  await serve(predecessor, db, async ({ api }) => {
    const archive = await api("games");
    for (const game of archive) {
      const facts = sql(db, `SELECT m.repertoire_id, m.ply, m.position_id, m.answer_version, m.played_move
        FROM mismatches m JOIN repertoire_choices c ON c.repertoire_id=m.repertoire_id
        WHERE m.game_id=${quote(game.id)} AND c.color=${quote(game.learner_color)} LIMIT 1`);
      if (!facts) continue;
      const [repertoire, ply, position, answer_version, played] = facts.split("|");
      selected = game;
      book = repertoire;
      legacyPly = Number(ply);
      const saved = { Repertoire: { ply: legacyPly, position, repertoire, answer_version, played } };
      const bytes = JSON.stringify(saved) + "\n";
      // Model a schema-18 scan after its first committed record, holding the
      // exact file and byte checkpoint needed to finish the next record.
      asset = "88887777666655554444333322221111";
      const staging = db.replace(/\.sqlite3$/, ".uploads");
      fs.mkdirSync(staging, { recursive: true });
      fs.writeFileSync(path.join(staging, asset), bytes + bytes);
      legacyJob = Number(sql(db, `INSERT INTO jobs(provider,username,operation,status)
        VALUES ('pgn','fixture','upload','done');
        INSERT INTO jobs(provider,username,operation,status,parent_id,detector_game,detector_version,upload_file,window_offset)
        VALUES ('archive','','compare_new','running',last_insert_rowid(),${quote(game.id)},1,${quote(asset)},${Buffer.byteLength(bytes)})
        RETURNING id;`));
      sql(db, `INSERT INTO job_games(job_id,game_id,scanned) VALUES (${legacyJob},${quote(game.id)},0);`);
      break;
    }
    assert(selected, "Retained first archive page needs a real game with earlier mismatch observations");
  }, false);
  const checkpoint = sql(db, "SELECT detector_version,detector_game,upload_file,window_offset FROM jobs WHERE id=" + legacyJob);
  execFileSync(candidate, ["migrate-games", db], { stdio: "pipe" });
  assert.equal(sql(db, "PRAGMA user_version"), "19");
  assert.equal(sql(db, "SELECT detector_version,detector_game,upload_file,window_offset FROM jobs WHERE id=" + legacyJob), checkpoint);
  assert.equal(sql(db, "PRAGMA integrity_check; PRAGMA foreign_key_check"), "ok");
  const page = await browser.newPage();
  await serve(candidate, db, async ({ origin, api }) => {
    await waitFor(() => jobStatus(db, legacyJob) === "done", "predecessor sealed scan");
    assert.equal(sql(db, "SELECT upload_file FROM jobs WHERE id=" + legacyJob), "");
    const older = await api(feedbackPath(selected.id, book));
    assert.equal(older.report, null);
    assert.equal(older.legacy_only, true);
    await enterGames(page, origin);
    await chooseGame(page, selected.white + " vs " + selected.black);
    const select = panel(page).getByRole("combobox", { name: "Repertoire", exact: true });
    await select.waitFor();
    const option = select.locator("option");
    while (!(await option.evaluateAll((rows, key) => rows.some(row => row.value === key), book))) {
      await panel(page).getByRole("button", { name: "More repertoires" }).click();
    }
    await chooseBook(page, book);
    await count(page, "Earlier mismatch findings are incomplete.");
    await panel(page).getByRole("button", { name: "Compare game", exact: true }).click();
    await count(page, "Compared with your current repertoire.");
    const report = (await api(feedbackPath(selected.id, book))).report;
    assert(report.followed + report.differed > 0, "retained game has assessed occurrences");
    assert.equal(report.followed + report.differed + report.unassessed, report.decisions.length);
    if (report.differed) {
      await panel(page).getByRole("button", { name: "First repertoire mismatch" }).click();
      const first = report.decisions.find(row => row.outcome === "differed");
      await page.getByText("Position " + first.ply + " of " + selected.route.moves.length, { exact: true }).waitFor();
    }
    console.log("Retained imported game compared in Chromium. " + report.followed + " followed, " +
      report.differed + " differed, " + report.unassessed + " unassessed. Schema-18 sealed work resumed.");
  });
  await page.close();
}

(async () => {
  const browser = await chromium.launch({ executablePath: process.env.TEMPO_QUALIFY_CHROME });
  try {
    await fixtureFlow(browser);
    if (predecessor) await predecessorFlow(browser);
  } finally {
    await browser.close();
    fs.rmSync(root, { recursive: true, force: true });
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
