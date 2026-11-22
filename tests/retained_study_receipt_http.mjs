// Run with TEMPO_PREVIOUS_BINARY and TEMPO_BINARY set to the exact old and new builds.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { DatabaseSync } from "node:sqlite";
import test from "node:test";

function binary(name) {
  assert.ok(process.env[name], `Set ${name} to the selected Tempo executable`);
  return resolve(process.env[name]);
}

async function serve(executable, database, ui, work) {
  const child = spawn(executable, ["serve", database, ui, "0"], {
    stdio: ["ignore", "pipe", "inherit"],
  });
  const lines = createInterface({ input: child.stdout });
  try {
    const [line] = await Promise.race([
      once(lines, "line"),
      once(child, "exit").then(([code]) => { throw new Error(`Tempo exited: ${code}`); }),
    ]);
    const url = line.replace(/^Tempo study at /, "");
    assert.match(url, /^http:\/\/127\.0\.0\.1:\d+$/);
    const post = async (route, bytes) => {
      const response = await fetch(url + route, {
        method: "POST", headers: { "Content-Type": "application/json" },
        body: bytes, signal: AbortSignal.timeout(4000),
      });
      assert.equal(response.ok, true);
      return response.json();
    };
    await work(post);
  } finally {
    lines.close();
    if (child.exitCode === null && child.signalCode === null) {
      const ended = once(child, "exit");
      child.kill();
      await ended;
    }
  }
}

function readState(database, submission) {
  const sql = new DatabaseSync(database, { readOnly: true, timeout: 5000 });
  try {
    const schema = sql.prepare("PRAGMA user_version").get().user_version;
    return {
      schema,
      days: schema >= 25 ? sql.prepare(
        "SELECT dataset_id,study_day,opening_spent,tactic_spent,spend_proven FROM study_day_authority"
      ).all() : [],
      attempts: sql.prepare("SELECT id,record FROM attempts ORDER BY id").all(),
      receipt: sql.prepare("SELECT * FROM receipts WHERE id=?").get(submission),
      assessmentCount: sql.prepare("SELECT count(*) AS n FROM assessments").get().n,
      learning: sql.prepare("SELECT target_id,state FROM learning ORDER BY target_id").all(),
    };
  } finally {
    sql.close();
  }
}

for (const priorAccepted of [true, false]) {
  test(`previous issued Teach recovers with ${priorAccepted ? "an accepted" : "no accepted"} receipt`,
    { timeout: 20000 }, async t => {
      const previous = binary("TEMPO_PREVIOUS_BINARY");
      const candidate = binary("TEMPO_BINARY");
      const root = await mkdtemp(join(tmpdir(), "tempo-retained-study-receipt-"));
      t.after(() => rm(root, { recursive: true, force: true }));
      const database = join(root, "study.sqlite");
      const ui = join(root, "ui");
      await mkdir(join(ui, "assets"), { recursive: true });
      await writeFile(join(ui, "index.html"), "<title>Receipt fixture</title>");
      execFileSync(previous, ["init", database]);
      const submission = "old-client-ack";
      let pending;
      let accepted;
      await serve(previous, database, ui, async post => {
        const { dataset_id } = await post("/api/repertoires", JSON.stringify({ command: "list" }));
        const input = { dataset_id, name: "Receipt fixture", learner_color: "white", pgn: "1. e4 *" };
        const preview = await post("/api/repertoires", JSON.stringify({ ...input, command: "preview" }));
        assert.ok(preview.token);
        const saved = await post("/api/repertoires", JSON.stringify({
          ...input, command: "save", token: preview.token,
        }));
        assert.equal(saved.repertoires.length, 1);
        const issued = await post("/api/study", JSON.stringify({ command: "next" }));
        pending = JSON.stringify({
          command: "acknowledge", dataset_id, attempt_id: issued.attempt_id,
          expected_revision: issued.revision, submission_id: submission,
          answer: { uci: issued.lesson.move_uci },
        });
        if (priorAccepted) {
          accepted = await post("/api/study", pending);
          assert.equal(accepted.status, "complete");
        }
      });
      const before = readState(database, submission);
      assert.equal(before.schema, 22);
      if (priorAccepted) {
        assert.ok(JSON.parse(before.receipt.result).Study.attempt);
      } else {
        assert.equal(before.receipt, undefined);
      }
      await serve(candidate, database, ui, async post => {
        if (!priorAccepted) {
          const sql = new DatabaseSync(database, { timeout: 5000 });
          try {
            // The acceptance must commit despite no projection head or state.
            sql.exec("DELETE FROM study_projection_state");
            const beforeFailure = readState(database, submission);
            sql.exec(`CREATE TRIGGER refuse_spend BEFORE UPDATE OF opening_spent ON study_day_authority
              BEGIN SELECT RAISE(ABORT, 'injected spend failure'); END`);
            const failure = await post("/api/study", pending);
            assert.equal(failure.status, "error");
            assert.deepEqual(readState(database, submission), beforeFailure);
            sql.exec("DROP TRIGGER refuse_spend");
          } finally {
            sql.close();
          }
        }
        const completed = await post("/api/study", pending);
        assert.equal(completed.status, "complete");
        assert.equal(completed.attempt_id, JSON.parse(pending).attempt_id);
        if (priorAccepted) assert.deepEqual(completed, accepted);
        const settled = readState(database, submission);
        assert.equal(settled.schema, 25);
        assert.equal(settled.assessmentCount, before.assessmentCount);
        const acceptance = JSON.parse(settled.attempts[0].record).acceptance;
        assert.deepEqual(settled.days.map(day => ({ ...day })), [{
          dataset_id: JSON.parse(pending).dataset_id,
          study_day: acceptance.at.day,
          opening_spent: 1,
          tactic_spent: 0,
          spend_proven: 1,
        }]);
        assert.deepEqual(await post("/api/study", pending), completed);
        assert.deepEqual(readState(database, submission), settled);
        if (priorAccepted) {
          assert.deepEqual(settled.receipt, before.receipt);
          assert.deepEqual(settled.learning, before.learning);

          const sql = new DatabaseSync(database, { timeout: 5000 });
          try {
            sql.exec("UPDATE learning SET state='not-json'");
            const unreadable = readState(database, submission);
            assert.deepEqual(await post("/api/study", pending), completed);
            assert.deepEqual(readState(database, submission), unreadable);
            const wrongDataset = { ...JSON.parse(pending), dataset_id: "wrong-dataset" };
            assert.equal((await post("/api/study", JSON.stringify(wrongDataset))).code, "stale_dataset");
          } finally {
            for (const row of settled.learning)
              sql.prepare("UPDATE learning SET state=? WHERE target_id=?").run(row.state, row.target_id);
            sql.close();
          }
        } else {
          assert.equal(JSON.parse(settled.receipt.result).Study.receipt_version, 1);
          assert.notDeepEqual(settled.learning, before.learning);
        }
      });
    });
}
