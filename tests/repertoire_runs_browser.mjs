// Play real Study moves through the packaged desktop and phone surfaces.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { createServer, request } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { DatabaseSync, backup } from "node:sqlite";

const args = process.argv.slice(2);
assert.equal(args.length, 2, "Usage: node repertoire_runs_browser.mjs BINARY UI");
const [binary, ui] = args.map(value => resolve(value));
const { chromium, webkit, devices } = await import(process.env.TEMPO_PLAYWRIGHT || "playwright");
const directory = await mkdtemp(join(tmpdir(), "tempo-line-browser-"));
const pgn = "1. e4 e5 2. Nf3 Nc6 3. Bb5 a6 4. Ba4 Nf6 5. O-O Be7 6. Re1 b5 7. Bb3 d6 8. c3 O-O 9. h3 Nb8 10. d4 *";
const phoneAuthority = "tempo-test.example.ts.net:8443";
const phoneOrigin = "https://" + phoneAuthority;
const login = "owner@example.invalid";
const endpoint = (request, command) => request.url().endsWith("/api/study")
  && request.postDataJSON()?.command === command;

async function start(database) {
  const child = spawn(binary, ["serve", database, ui, "0"], {
    env: { ...process.env, TEMPO_PHONE_ORIGIN: phoneOrigin, TEMPO_PHONE_TAILSCALE_LOGIN: login,
      TEMPO_LOG_DIR: join(directory, "diagnostics") },
  });
  const output = createInterface({ input: child.stdout });
  let errors = "";
  child.stderr.on("data", chunk => { errors += chunk; });
  const stop = async () => {
    if (child.exitCode !== null || child.signalCode !== null) return;
    const exited = once(child, "exit");
    child.kill();
    await exited;
  };
  let timer;
  try {
    const [banner] = await Promise.race([
      once(output, "line"),
      once(child, "exit").then(([code]) => { throw Error(`Tempo exited (${code}). ${errors}`); }),
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(Error("Tempo startup timed out. " + errors)), 10000);
      }),
    ]);
    const origin = /^Tempo study at (http:\/\/127\.0\.0\.1:\d+)$/.exec(banner)?.[1];
    assert.ok(origin, errors || banner);
    return { origin, stop };
  } catch (error) {
    await stop();
    throw error;
  } finally {
    clearTimeout(timer);
    output.close();
  }
}

async function post(host, route, payload) {
  const response = await fetch(host.origin + "/api/" + route, {
    method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(payload),
  });
  const view = await response.json();
  assert.equal(response.status, 200, JSON.stringify(view));
  assert.notEqual(view.status, "error", JSON.stringify(view));
  return view;
}

function address(command, view, submission_id, uci) {
  return { command, dataset_id: view.dataset_id, attempt_id: view.attempt_id,
    expected_revision: view.revision, submission_id, ...(uci && { answer: { uci } }) };
}

function withDatabase(database, run, readOnly = false) {
  const db = new DatabaseSync(database, { readOnly });
  try { return run(db); } finally { db.close(); }
}

function setDue(database, today, due) {
  withDatabase(database, db => {
    const update = db.prepare("UPDATE learning SET state=json_set(state,'$.due',CAST(? AS INTEGER)) WHERE target_id=?");
    for (const { target_id } of db.prepare("SELECT target_id FROM learning").all())
      update.run(due.includes(target_id) ? today : today + 7, target_id);
  });
}

async function seed() {
  // Observe ordinary Queue order in the real server before choosing a fixture
  // window. Closing that process clears only the setup session's deferrals.
  for (let trial = 0; trial < 10; trial++) {
    const database = join(directory, "seed-" + trial + ".sqlite");
    execFileSync(binary, ["init", database]);
    let host = await start(database);
    try {
      const { dataset_id } = await post(host, "repertoires", { command: "list" });
      const input = { dataset_id, pgn, name: "Line fixture", learner_color: "white" };
      const { token } = await post(host, "repertoires", { command: "preview", ...input });
      await post(host, "repertoires", { command: "save", token, ...input });
      for (let i = 0; i < 10; i++) {
        const lesson = await post(host, "study", { command: "next" });
        assert.equal(lesson.activity, "teach");
        const ack = address("acknowledge", lesson, "teach-" + i, lesson.lesson.move_uci);
        assert.equal((await post(host, "study", ack)).status, "complete");
      }
    } finally { await host.stop(); }
    const { targets, today } = withDatabase(database, db => ({
      targets: db.prepare(
        `SELECT t.target_id AS id, json_extract(c.item,'$.definition.decisions[0].expected.uci') AS move
          FROM line_targets t JOIN current_content c USING (target_id) ORDER BY t.ordinal`,
      ).all(),
      today: db.prepare("SELECT json_extract(state,'$.first_introduced') AS day FROM learning LIMIT 1").get().day,
    }), true);
    setDue(database, today, targets.map(target => target.id));
    const rank = new Map();
    host = await start(database);
    try {
      for (let i = 0; i < targets.length; i++) {
        const view = await post(host, "study", { command: "next" });
        assert.equal(view.activity, "test");
        rank.set(view.target_id, i);
        await post(host, "study", address("skip", view, "seed-skip-" + i));
      }
    } finally { await host.stop(); }
    const anchor = targets.findIndex((target, i) => i > 0 && i + 2 < targets.length
      && targets.slice(i + 1, i + 3).every(next => rank.get(target.id) < rank.get(next.id)));
    if (anchor < 0) continue;
    const warmup = targets[anchor - 1];
    const tests = targets.slice(anchor, anchor + 3);
    setDue(database, today, [warmup.id]);
    host = await start(database);
    try {
      const parent = await post(host, "study", { command: "next" });
      assert.equal(parent.target_id, warmup.id);
      const result = await post(host, "study", address("answer", parent, "master", warmup.move));
      assert.equal(result.assessment, "good");
    } finally { await host.stop(); }
    setDue(database, today, tests.map(test => test.id));
    return { database, steps: [warmup, ...tests] };
  }
  assert.fail("No consecutive ordinary-review fixture window");
}

async function phoneProxy(host) {
  const proxy = createServer((incoming, outgoing) => {
    const headers = { ...incoming.headers, host: phoneAuthority, "x-forwarded-host": phoneAuthority,
      "x-forwarded-proto": "https", "tailscale-user-login": login };
    if (headers.origin) headers.origin = phoneOrigin;
    const upstream = request(host.origin + incoming.url, { method: incoming.method, headers }, response => {
      outgoing.writeHead(response.statusCode, response.headers);
      response.pipe(outgoing);
    });
    upstream.on("error", () => outgoing.writeHead(502).end());
    incoming.pipe(upstream);
  });
  proxy.listen(0, "127.0.0.1");
  await once(proxy, "listening");
  return { origin: "http://127.0.0.1:" + proxy.address().port,
    stop: () => new Promise(resolve => proxy.close(resolve)) };
}

function reply(page, command) {
  return page.waitForResponse(response => endpoint(response.request(), command) && response.status() === 200)
    .then(response => response.json());
}

async function move(page, view, uci, phone) {
  await page.waitForFunction(fen =>
    document.querySelector('[aria-label="Study chessboard"][aria-disabled="false"]')?.dataset.fen === fen, view.display_fen);
  const board = page.locator(".study-board cg-board");
  await board.scrollIntoViewIfNeeded();
  const box = await board.boundingBox();
  assert.ok(box);
  for (const offset of [0, 2]) {
    const x = box.x + (uci.charCodeAt(offset) - 97 + 0.5) * box.width / 8;
    const y = box.y + (8 - Number(uci[offset + 1]) + 0.5) * box.height / 8;
    await (phone ? page.touchscreen.tap(x, y) : page.mouse.click(x, y));
  }
}

const copy = async (source, destination) => {
  const db = new DatabaseSync(source, { readOnly: true });
  try { await backup(db, destination); } finally { db.close(); }
};

async function journey(seed, name, browser, kind) {
  const phone = name === "phone";
  const database = join(directory, name + ".sqlite");
  await copy(seed.database, database);
  const host = await start(database);
  const proxy = phone ? await phoneProxy(host) : null;
  const page = await browser.newPage(phone ? devices["iPhone 15"] : { viewport: { width: 1440, height: 900 } });
  page.setDefaultTimeout(10000);
  const requests = [];
  const errors = [];
  let failNext = false;
  let loseAnswer = false;
  page.on("pageerror", error => errors.push(error.message));
  await page.route("**/api/study", async route => {
    const bytes = route.request().postData();
    const { command } = JSON.parse(bytes);
    requests.push({ command, bytes });
    if (failNext && command === "next") {
      failNext = false;
      await route.abort("failed");
    } else if (loseAnswer && command === "answer") {
      loseAnswer = false;
      const accepted = await route.fetch();
      assert.equal((await accepted.json()).assessment, "good");
      await route.abort("failed");
    } else {
      await route.continue();
    }
  });
  const button = name => page.getByRole("button", { name, exact: true });
  const board = page.locator(".study-board cg-board");
  const grades = () => withDatabase(database, db => db.prepare("SELECT count(*) AS total FROM assessments").get().total, true);
  try {
    const first = reply(page, "next");
    await page.goto((proxy?.origin || host.origin) + "#study");
    let view = await first;
    assert.equal(view.repertoire_context, "warmup");
    assert.equal(view.target_id, seed.steps[0].id);
    await page.getByRole("heading", { name: "Repertoire warm-up" }).waitFor();
    await board.evaluate(node => node.dataset.connectedJourney = "same-board");
    if (kind === "skip") {
      const next = reply(page, "next");
      await button("Skip warm-up").click();
      view = await next;
      assert.equal(view.target_id, seed.steps[1].id);
      assert.equal(grades(), 1);
      assert.deepEqual(requests.map(item => item.command), ["next", "skip", "next"]);
      return;
    }
    if (kind === "miss") {
      const wrong = view.legal_moves.find(uci => uci.length === 4 && uci !== seed.steps[0].move);
      const response = reply(page, "answer");
      await move(page, view, wrong, phone);
      view = await response;
      assert.equal(view.assessment, "again");
      const fixed = reply(page, "answer");
      await move(page, view, seed.steps[0].move, phone);
      assert.equal((await fixed).assessment, "again");
      await page.getByText("Warm-up completed after correction.", { exact: true }).waitFor();
      assert.equal(grades(), 1);
      const next = reply(page, "next");
      await button("Continue").click();
      view = await next;
      assert.equal(view.target_id, seed.steps[1].id);
      return;
    }
    for (const [i, step] of seed.steps.entries()) {
      assert.equal(view.target_id, step.id);
      assert.equal(view.activity, i === 0 ? "practice" : "test");
      assert.equal(await board.getAttribute("data-connected-journey"), "same-board");
      if (i > 0) {
        assert.equal(typeof view.opponent_move?.before_fen, "string");
        assert.equal(view.display_fen, view.prompt.fen);
      }
      const interruptNext = !phone && i === 1;
      const interruptAnswer = !phone && i === 2;
      failNext = interruptNext;
      loseAnswer = interruptAnswer;
      const accepted = reply(page, "answer");
      let next = i < 3 && !interruptNext ? reply(page, "next") : null;
      let uncertain;
      await move(page, view, step.move, phone);
      if (interruptAnswer) {
        await button("Retry").waitFor();
        uncertain = requests.at(-1);
        assert.equal(grades(), i + 1);
        await button("Retry").click();
      }
      const result = await accepted;
      if (uncertain) assert.deepEqual(requests.at(-1), uncertain);
      assert.equal(result.assessment, "good");
      if (interruptNext) {
        await button("Retry next exercise").waitFor();
        await page.getByText("Review recorded as Good.", { exact: true }).waitFor();
        const offset = requests.length;
        next = reply(page, "next");
        await button("Retry next exercise").click();
        view = await next;
        assert.deepEqual(requests.slice(offset).map(item => item.command), ["next"]);
      } else if (next) {
        view = await next;
      }
      assert.equal(grades(), i + 1);
    }
    assert.deepEqual(errors, []);
  } finally {
    await page.close();
    await proxy?.stop();
    await host.stop();
  }
}

try {
  const prepared = await seed();
  const desktop = await chromium.launch({ channel: "chrome", headless: true });
  try {
    await journey(prepared, "desktop", desktop, "clean");
    await journey(prepared, "skip", desktop, "skip");
    await journey(prepared, "miss", desktop, "miss");
  } finally { await desktop.close(); }
  const phone = await webkit.launch();
  try { await journey(prepared, "phone", phone, "clean"); } finally { await phone.close(); }
  console.log("Desktop and phone connected reviews passed, including warm-up skip, correction, and exact recovery.");
} finally {
  await rm(directory, { recursive: true, force: true });
}
