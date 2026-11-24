// Run with the installed previous binary and the exact candidate binary.
// Set TEMPO_UI_DIST and TEMPO_PLAYWRIGHT to include desktop recovery.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { DatabaseSync } from "node:sqlite";
import test from "node:test";

const previous = process.env.TEMPO_PREVIOUS_BINARY;
const candidate = process.env.TEMPO_BINARY;
assert.ok(previous && candidate, "Set both exact Tempo binaries");
const recoveryKey = "tempo-repertoire-recovery-v4";

async function serve(binary, database, ui, work) {
  const child = spawn(resolve(binary), ["serve", database, ui, "0"], { stdio: ["ignore", "pipe", "inherit"] });
  const output = createInterface({ input: child.stdout });
  try {
    const url = await Promise.race([
      (async () => {
        for await (const line of output) {
          const match = /^Tempo study at (http:\/\/127\.0\.0\.1:\d+)$/.exec(line);
          if (match) return match[1];
        }
        throw new Error("Tempo did not report a listener");
      })(),
      once(child, "exit").then(([code]) => { throw new Error(`Tempo exited: ${code}`); }),
    ]);
    const post = async (route, command) => {
      const response = await fetch(url + route, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(command),
        signal: AbortSignal.timeout(5_000),
      });
      assert.equal(response.status, 200);
      return response.json();
    };
    await work(post, url);
  } finally {
    output.close();
    if (child.exitCode === null && child.signalCode === null) {
      const ended = once(child, "exit");
      child.kill();
      await ended;
    }
  }
}

function schema(database) {
  const db = new DatabaseSync(database, { readOnly: true });
  try {
    return db.prepare("PRAGMA user_version").get().user_version;
  } finally {
    db.close();
  }
}

function authority(database) {
  const db = new DatabaseSync(database, { readOnly: true });
  try {
    return ["store_state", "current_content", "learning", "attempts", "assessments", "receipts",
      "repertoire_resets"].map(table => db.prepare(`SELECT * FROM ${table} ORDER BY rowid`).all());
  } finally {
    db.close();
  }
}

test("previous Reset replay is exact and fresh destructive commands are retired", async () => {
  const directory = await mkdtemp(join(tmpdir(), "tempo-reset-retire-"));
  const database = join(directory, "study.sqlite");
  const ui = process.env.TEMPO_UI_DIST || join(directory, "fixture-ui");
  let accepted, originalReply, otherRepertoire;
  try {
    if (!process.env.TEMPO_UI_DIST) {
      await mkdir(join(ui, "assets"), { recursive: true });
      await writeFile(join(ui, "index.html"), "<!doctype html><title>Disposable UI</title>");
    }
    execFileSync(resolve(previous), ["init", database]);
    await serve(previous, database, ui, async (post) => {
      const repertoire = command => post("/api/repertoires", command);
      const { dataset_id } = await repertoire({ command: "list" });
      async function authored(name, pgn) {
        const input = { dataset_id, name, learner_color: "white", pgn };
        const preview = await repertoire({ ...input, command: "preview" });
        assert.ok(preview.token, JSON.stringify(preview));
        const saved = await repertoire({ ...input, command: "save", token: preview.token });
        return saved.repertoires.find(item => item.name === name).id;
      }
      const repertoire_id = await authored("Reset fixture", "1. e4 *");
      otherRepertoire = await authored("Retained second book", "1. d4 *");
      accepted = { command: "reset_progress", dataset_id, repertoire_id, command_id: "old-reset" };
      originalReply = await repertoire(accepted);
      assert.deepEqual(originalReply, { schema_version: 4, status: "progress_reset", dataset_id, repertoire_id });

      const study = command => post("/api/study", command);
      const lesson = await study({ command: "next" });
      assert.equal(lesson.activity, "teach");
      const completed = await study({
        command: "acknowledge", dataset_id, attempt_id: lesson.attempt_id,
        expected_revision: lesson.revision, submission_id: "newer-teach",
        answer: { uci: lesson.lesson.move_uci },
      });
      assert.equal(completed.status, "complete", JSON.stringify(completed));
      const active = await study({ command: "next" });
      assert.equal(active.status, "ready", JSON.stringify(active));
    });
    assert.equal(schema(database), 22);
    await serve(candidate, database, ui, async (post, url) => {
      assert.equal(schema(database), 28);
      const before = authority(database);
      const repertoire = command => post("/api/repertoires", command);
      const fresh = { ...accepted, command_id: "unaccepted-reset" };
      assert.deepEqual(await repertoire(accepted), originalReply);
      const retired = await repertoire(fresh);
      assert.equal(retired.code, "retired_feature");
      assert.equal(retired.retryable, false);
      assert.equal((await repertoire({ ...accepted, repertoire_id: otherRepertoire })).code, "command_conflict");
      assert.equal((await repertoire({ ...accepted, dataset_id: "other" })).code, "stale_dataset");
      assert.deepEqual(authority(database), before);

      if (process.env.TEMPO_UI_DIST && process.env.TEMPO_PLAYWRIGHT) {
        const { chromium } = await import(process.env.TEMPO_PLAYWRIGHT);
        const browser = await chromium.launch({ channel: "chrome", headless: true });
        try {
          for (const command of [accepted, fresh]) {
            const page = await browser.newPage();
            await page.addInitScript(({ key, command }) => sessionStorage.setItem(key, JSON.stringify(command)),
              { key: recoveryKey, command });
            const sent = [];
            page.on("request", req => {
              if (req.url().endsWith("/api/repertoires") && req.method() === "POST")
                sent.push(req.postDataJSON());
            });
            await page.goto(url);
            await page.getByRole("button", { name: "Repertoires", exact: true }).click();
            await page.getByRole("button", { name: "Edit Reset fixture", exact: true }).waitFor();
            assert.deepEqual(sent.filter(item => item.command === "reset_progress"), [command]);
            assert.equal(await page.getByRole("button", { name: /^Reset progress/ }).count(), 0);
            assert.equal(await page.evaluate(key => sessionStorage.getItem(key), recoveryKey), null);
            await page.close();
          }
        } finally {
          await browser.close();
        }
        assert.deepEqual(authority(database), before);
      }
    });
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
