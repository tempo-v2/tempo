// Compare the retired connected-run header and recovery over the real HTTP boundary.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { DatabaseSync, backup } from "node:sqlite";

const args = process.argv.slice(2);
assert.equal(args.length, 3, "Usage: node connected_run_retirement_http.mjs PREVIOUS_BINARY CANDIDATE_BINARY UI");
const [previous, candidate, ui] = args.map(value => resolve(value));
const directory = await mkdtemp(join(tmpdir(), "tempo-connected-run-http-"));
const pgn = "1. e4 e5 2. Nf3 Nc6 3. Bb5 a6 4. Ba4 Nf6 5. O-O Be7 6. Re1 b5 7. Bb3 d6 8. c3 O-O 9. h3 Nb8 10. d4 *";
const runHeader = { "X-Tempo-Study-Repertoire-Runs": "1" };

async function start(binary, database) {
  const child = spawn(binary, ["serve", database, ui, "0"], {
    env: {
      ...Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith("TEMPO_PHONE_"))),
      TEMPO_LOG_DIR: join(directory, "diagnostics"),
    },
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

async function post(host, route, payload, headers = {}) {
  const response = await fetch(host.origin + "/api/" + route, {
    method: "POST", headers: { "content-type": "application/json", ...headers }, body: JSON.stringify(payload),
    signal: AbortSignal.timeout(5000),
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
    execFileSync(previous, ["init", database]);
    let host = await start(previous, database);
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
    host = await start(previous, database);
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
    host = await start(previous, database);
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


async function copy(source, destination) {
  const db = new DatabaseSync(source, { readOnly: true });
  try { await backup(db, destination); } finally { db.close(); }
}

function facts(database) {
  return withDatabase(database, db => ({
    attempts: db.prepare("SELECT record FROM attempts ORDER BY id").all(),
    receipts: db.prepare("SELECT id, command, result FROM receipts ORDER BY id").all(),
    assessments: db.prepare("SELECT * FROM assessments ORDER BY ordinal").all(),
    learning: db.prepare("SELECT * FROM learning ORDER BY target_id").all(),
    run: db.prepare("SELECT * FROM repertoire_run_steps ORDER BY ordinal").all(),
  }), true);
}

try {
  const { database: baseline, steps } = await seed();
  const replies = [];
  for (const [index, headers] of [{}, runHeader].entries()) {
    const database = join(directory, "candidate-" + index + ".sqlite");
    await copy(baseline, database);
    const before = facts(database);
    const host = await start(candidate, database);
    try {
      const view = await post(host, "study", { command: "next" }, headers);
      assert.equal(view.activity, "test");
      assert.equal(view.target_id, steps[1].id);
      assert.ok(!("repertoire_context" in view));
      const { attempt_id, ...comparable } = view;
      assert.ok(attempt_id);
      replies.push(comparable);
    } finally { await host.stop(); }
    const after = facts(database);
    assert.equal(after.attempts.length, before.attempts.length + 1);
    assert.deepEqual(after.assessments, before.assessments);
    assert.deepEqual(after.learning, before.learning);
    assert.deepEqual(after.receipts, before.receipts);
    assert.deepEqual(after.run, []);
  }
  assert.deepEqual(replies[0], replies[1]);
  console.log("Legacy header present/absent issues the same standalone Test.");

  const database = join(directory, "prior-receipt.sqlite");
  await copy(baseline, database);
  const legacy = await start(previous, database);
  let pending;
  let priorReply;
  let issuedTest;
  try {
    const warmup = await post(legacy, "study", { command: "next" }, runHeader);
    assert.equal(warmup.repertoire_context, "warmup");
    assert.equal(warmup.target_id, steps[0].id);
    pending = address("answer", warmup, "prior-warmup-answer", steps[0].move);
    priorReply = await post(legacy, "study", pending, runHeader);
    assert.equal(priorReply.status, "complete");
    issuedTest = await post(legacy, "study", { command: "next" }, runHeader);
    assert.equal(issuedTest.activity, "test");
    assert.equal(issuedTest.target_id, steps[1].id);
  } finally { await legacy.stop(); }
  const before = facts(database);
  assert.ok(before.run.length > 1);
  const upgraded = await start(candidate, database);
  try {
    const replay = await post(upgraded, "study", pending, runHeader);
    const { repertoire_context, ...issued } = priorReply;
    assert.equal(repertoire_context, "warmup");
    assert.deepEqual(replay, issued);
    const resumed = await post(upgraded, "study", { command: "next" });
    assert.equal(resumed.attempt_id, issuedTest.attempt_id);
    assert.equal(resumed.content_version, issuedTest.content_version);
    assert.equal(resumed.revision, issuedTest.revision);
  } finally { await upgraded.stop(); }
  assert.deepEqual(facts(database), before);
  console.log("Previous installed warm-up receipt replayed; issued Test resumed without new writes.");
} finally {
  await rm(directory, { recursive: true, force: true });
}
