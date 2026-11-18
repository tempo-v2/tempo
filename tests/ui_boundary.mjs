import test from 'node:test';
import assert from 'node:assert/strict';
import { createStudyController, renderStudy, validateStudyView } from '../../tempo-ui/index.js';
import { stdioTransport } from '../demo/stdio-transport.mjs';

test('actual Rust application replies satisfy UI contract through a complete study journey', async t => {
  const transport = stdioTransport();
  t.after(() => transport.close());
  const rendered = [];
  let sequence = 0;
  const ui = createStudyController({ transport, render: view => rendered.push(renderStudy(view)), storage: { load: () => null, save() {} }, makeSubmissionId: () => `ui-${++sequence}` });
  let view = await ui.next();
  assert.equal(view.target_id, 'r1:p1');
  assert.equal(view.status, 'ready');
  assert.ok(!JSON.stringify(view).includes('expected'));
  view = await ui.answer('e2e4');
  assert.equal(view.assessment, 'good');
  assert.equal(view.status, 'complete');
  assert.deepEqual(view.feedback.autoplay, [{ uci: 'e7e5' }]);
  assert.equal((await ui.next()).target_id, 'r1:p2');
  await ui.answer('g1f3');
  assert.equal((await ui.next()).target_id, 't1');
  view = await ui.answer('e2e4');
  assert.equal(view.status, 'ready');
  assert.equal(view.assessment, null);
  assert.equal(view.revision, 1);
  view = await ui.answer('g1f3');
  assert.equal(view.status, 'complete');
  assert.equal(view.assessment, 'good');
  assert.equal((await ui.next()).status, 'empty');
  assert.ok(rendered.length >= 8);
});

test('lost reply retries the exact intent against the real persisted receipt', async t => {
  const actual = stdioTransport();
  t.after(() => actual.close());
  const commands = [];
  let lose = true;
  const transport = { async send(command) {
    commands.push(structuredClone(command));
    const result = await actual.send(command);
    if (command.command === 'answer' && lose) { lose = false; throw new Error('simulated reply loss after commit'); }
    return result;
  } };
  let sequence = 0;
  const ui = createStudyController({ transport, render() {}, storage: { load: () => null, save() {} }, makeSubmissionId: () => `retry-${++sequence}` });
  await ui.next();
  await assert.rejects(ui.answer('e2e4'), /reply loss/);
  assert.equal(ui.getState().canRetry, true);
  const result = await ui.retry();
  assert.equal(result.assessment, 'good');
  assert.equal(result.revision, 1);
  assert.deepEqual(commands[1], commands[2]);
  assert.equal((await ui.next()).target_id, 'r1:p2');
});

test('invalid command returns a typed error without killing the Rust process', async t => {
  const transport = stdioTransport();
  t.after(() => transport.close());
  const error = await transport.send({ command: 'erase_everything' });
  validateStudyView(error);
  assert.equal(error.status, 'error');
  assert.equal(error.code, 'invalid_command');
  assert.equal((await transport.send({ command: 'next' })).status, 'ready');
});
