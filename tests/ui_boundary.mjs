import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { DatabaseSync } from 'node:sqlite';
import { pathToFileURL } from 'node:url';

const { createStudyController, renderStudy, validateStudyView } =
  await import(pathToFileURL(resolve('tempo-ui/index.js')).href);
const binary = resolve(process.env.TEMPO_BINARY || 'target/debug/tempo');

// Lichess CC0 tactic 03ySM, after Black's b6d4.
const tactic = {
  namespace: 'ui-boundary',
  source_id: '03ySM',
  solver_fen: '2r3k1/5pp1/Q3b2p/3p4/3q4/3B3P/1P3PP1/4R1K1 w - - 0 27',
  solution: ['a6c8', 'e6c8', 'e1e8'],
  label: 'Mate in two',
  provenance: ['Lichess CC0'],
};

async function httpFixture(t) {
  const directory = await mkdtemp(join(tmpdir(), 'tempo-ui-boundary-'));
  let child;
  t.after(async () => {
    if (child && child.exitCode === null && child.signalCode === null && child.pid) {
      const stopped = once(child, 'exit');
      child.kill();
      await stopped;
    }
    await rm(directory, { recursive: true, force: true });
  });

  const database = join(directory, 'study.sqlite');
  const transfer = join(directory, 'tactics.json');
  const assets = join(directory, 'ui');
  await mkdir(join(assets, 'assets'), { recursive: true });
  await writeFile(join(assets, 'index.html'), '<title>Study contract fixture</title>');
  await writeFile(transfer, JSON.stringify({ schema_version: 1, puzzles: [tactic] }));
  execFileSync(binary, ['init', database], { cwd: directory, timeout: 5000 });
  execFileSync(binary, ['import-tactics', database, transfer], { cwd: directory, timeout: 5000 });
  child = spawn(binary, ['serve', database, assets, '0'], {
    cwd: directory,
    stdio: ['ignore', 'pipe', 'inherit'],
  });
  const lines = createInterface({ input: child.stdout });
  const [line] = await Promise.race([
    once(lines, 'line', { signal: AbortSignal.timeout(5000) }),
    once(child, 'exit').then(([code]) => { throw new Error(`Tempo exited during startup (${code})`); }),
  ]);
  const origin = line.match(/^Tempo study at (http:\/\/127\.0\.0\.1:\d+)$/)?.[1];
  assert.ok(origin, line);
  return {
    database,
    async send(command) {
      const response = await fetch(origin + '/api/study', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(command),
        signal: AbortSignal.timeout(5000),
      });
      assert.equal(response.status, 200);
      return response.json();
    },
  };
}

function controller(transport, render = () => {}) {
  let sequence = 0;
  return createStudyController({
    transport,
    render,
    storage: { load: () => null, save() {} },
    makeSubmissionId: () => `ui-${++sequence}`,
  });
}

function acceptedWrites(database) {
  const sql = new DatabaseSync(database, { readOnly: true });
  try {
    return ['assessments', 'receipts'].map(table => sql.prepare(`SELECT COUNT(*) AS total FROM ${table}`).get().total);
  } finally {
    sql.close();
  }
}

test('managed HTTP serves a complete Study journey through the UI contract', { timeout: 15000 }, async t => {
  const transport = await httpFixture(t);
  const rendered = [];
  const ui = controller(transport, view => rendered.push(renderStudy(view)));
  let view = await ui.next();
  assert.equal(view.activity, 'test');
  assert.equal(view.status, 'ready');
  assert.ok(!JSON.stringify(view).includes('expected'));
  view = await ui.answer('a6c8');
  assert.equal(view.status, 'ready');
  assert.equal(view.assessment, null);
  assert.equal(view.revision, 1);
  assert.deepEqual(view.feedback.autoplay, [{ uci: 'e6c8' }]);
  view = await ui.answer('e1e8');
  assert.equal(view.status, 'complete');
  assert.equal(view.assessment, 'good');
  assert.equal((await ui.next()).status, 'empty');
  assert.ok(rendered.length >= 8);
});

test('lost reply retries the exact command against the persisted HTTP receipt', { timeout: 15000 }, async t => {
  const actual = await httpFixture(t);
  const commands = [];
  let lose = true;
  const ui = controller({ async send(command) {
    commands.push(structuredClone(command));
    const result = await actual.send(command);
    if (command.command === 'answer' && command.expected_revision === 1 && lose) {
      lose = false;
      throw new Error('simulated reply loss after commit');
    }
    return result;
  } });
  await ui.next();
  await ui.answer('a6c8');
  await assert.rejects(ui.answer('e1e8'), /reply loss/);
  assert.equal(ui.getState().canRetry, true);
  assert.deepEqual(acceptedWrites(actual.database), [1, 2]);
  const result = await ui.retry();
  assert.equal(result.assessment, 'good');
  assert.equal(result.revision, 2);
  assert.deepEqual(commands[2], commands[3]);
  assert.deepEqual(acceptedWrites(actual.database), [1, 2]);
  assert.equal((await ui.next()).status, 'empty');
});

test('invalid HTTP command returns a typed error and leaves Study available', { timeout: 15000 }, async t => {
  const transport = await httpFixture(t);
  const error = await transport.send({ command: 'erase_everything' });
  validateStudyView(error);
  assert.equal(error.status, 'error');
  assert.equal(error.code, 'invalid_command');
  assert.equal((await transport.send({ command: 'next' })).status, 'ready');
});

test('the retired stdio command cannot start a fixture runtime', () => {
  const run = spawnSync(binary, ['stdio'], { encoding: 'utf8', input: '{"command":"next"}\n', timeout: 5000 });
  assert.ifError(run.error);
  assert.equal(run.status, 1);
  assert.equal(run.stdout, '');
  assert.match(run.stderr, /standalone stdio workflow has been retired/);
});
