// From the assembled workspace, after building Tempo and the UI:
//   node --test tempo/tests/diagnostics_recovery.mjs
// TEMPO_BIN and TEMPO_UI may select already-built artifacts. All data is disposable.
// This runs the real shared Study transport, HTTP server and SQLite adapter;
// withholding a committed response body simulates uncertain browser delivery.
import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { DatabaseSync } from 'node:sqlite';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { studyTransport } from '../../tempo-ui/study_transport.js';
import { createStudyController } from '../../tempo-ui/study_controller.js';
import { clearDiagnostics, diagnosticsSnapshot } from '../../tempo-ui/diagnostics.js';

const binary = path.resolve(process.env.TEMPO_BIN ??
    fileURLToPath(new URL('../../target/debug/tempo', import.meta.url)));
const ui = path.resolve(process.env.TEMPO_UI ??
    fileURLToPath(new URL('../../tempo-ui/dist', import.meta.url)));

async function bounded(promise, milliseconds, message) {
    let timer;
    try {
        return await Promise.race([promise, new Promise((_, reject) => {
            timer = setTimeout(() => reject(new Error(message)), milliseconds);
        })]);
    } finally {
        clearTimeout(timer);
    }
}

async function stop(child) {
    if (!child?.pid || child.exitCode !== null || child.signalCode !== null) return;
    const exited = once(child, 'exit');
    child.kill();
    const force = setTimeout(() => child.kill('SIGKILL'), 2000);
    try {
        await bounded(exited, 4000, 'Disposable Tempo did not exit');
    } finally {
        clearTimeout(force);
        child.stdout.destroy();
        child.stderr.destroy();
    }
}

async function start(database, logs) {
    const env = { ...process.env, TEMPO_LOG: 'debug', TEMPO_LOG_DIR: logs };
    delete env.TEMPO_PHONE_ORIGIN;
    delete env.TEMPO_PHONE_TAILSCALE_LOGIN;
    const child = spawn(binary, ['serve', database, ui, '0'], { env });
    let stderr = '';
    child.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-8192); });
    const output = readline.createInterface({ input: child.stdout });
    try {
        const banner = await bounded(Promise.race([
            once(output, 'line').then(([line]) => line),
            once(child, 'exit').then(() => { throw new Error(`Tempo exited before startup: ${stderr}`); }),
        ]), 8000, 'Disposable Tempo startup timed out');
        const match = /^Tempo study at (http:\/\/127\.0\.0\.1:\d+)$/.exec(banner);
        assert.ok(match, 'Diagnostics must preserve the startup stdout protocol');
        return { child, url: match[1] };
    } catch (error) {
        await stop(child);
        throw error;
    } finally {
        output.close();
    }
}

function persisted(database) {
    const db = new DatabaseSync(database, { readOnly: true });
    try {
        return {
            grades: db.prepare("SELECT json_extract(record, '$.input.grade') AS grade FROM assessments ORDER BY ordinal")
                .all().map(row => row.grade),
            receipts: db.prepare('SELECT count(*) AS total FROM receipts').get().total,
            attempts: db.prepare('SELECT count(*) AS total FROM attempts').get().total,
            revision: db.prepare('SELECT hex(revision) AS revision FROM store_state').get().revision,
        };
    } finally {
        db.close();
    }
}

async function logEvents(directory) {
    const files = (await readdir(directory)).filter(name => /^tempo-[0-9a-f-]+\.jsonl$/.test(name));
    const groups = await Promise.all(files.map(async name => {
        const content = await readFile(path.join(directory, name), 'utf8');
        assert.ok(Buffer.byteLength(content) <= 5 * 1024 * 1024, 'Test logs remain bounded');
        return content.split('\n').slice(0, -1).filter(Boolean).map(JSON.parse);
    }));
    return groups.flat();
}

test('diagnostic deliveries preserve a committed Answer through body timeout and restart replay',
    { timeout: 30_000 }, async () => {
        const dir = await mkdtemp(path.join(tmpdir(), 'tempo-diagnostic-recovery-'));
        const database = path.join(dir, 'study.sqlite');
        const transfer = path.join(dir, 'normalized.json');
        const logs = path.join(dir, 'logs');
        const networkFetch = globalThis.fetch;
        let host;
        let loseBody = false;
        let lostResult;
        let saved = null;
        let sequence = 0;
        const deliveries = [];
        const client = () => createStudyController({
            transport: studyTransport,
            storage: {
                load: () => saved && JSON.parse(saved),
                save: envelope => { saved = JSON.stringify(envelope); },
            },
            render() {},
            makeSubmissionId: () => `private-review-command-${++sequence}`,
        });
        try {
            clearDiagnostics();
            // Same public Lichess 03ySM position used by tactics_http.mjs.
            await writeFile(transfer, JSON.stringify({ schema_version: 1, puzzles: [{
                namespace: 'tempo-review', source_id: '03ySM',
                solver_fen: '2r3k1/5pp1/Q3b2p/3p4/3q4/3B3P/1P3PP1/4R1K1 w - - 0 27',
                solution: ['a6c8', 'e6c8', 'e1e8'], label: 'Private review label',
                provenance: ['Private source'],
            }] }));
            const options = { timeout: 5000, maxBuffer: 512 * 1024 };
            execFileSync(binary, ['init', database], options);
            execFileSync(binary, ['import-tactics', database, transfer], options);
            host = await start(database, logs);
            globalThis.fetch = async (route, init) => {
                assert.equal(route, '/api/study');
                const delivery = { body: init.body, id: init.headers['X-Tempo-Request-Id'] };
                deliveries.push(delivery);
                const response = await networkFetch(host.url + route, init);
                assert.equal(response.headers.get('X-Tempo-Request-Id'), delivery.id);
                delivery.run = response.headers.get('X-Tempo-Run-Id');
                assert.match(delivery.run, /^[0-9a-f]{1,16}-[0-9]{1,10}-[0-9a-f]{1,16}$/);
                if (loseBody) {
                    loseBody = false;
                    lostResult = await response.json();
                    return {
                        ok: response.ok, status: response.status, headers: response.headers,
                        json: () => new Promise(() => {}),
                    };
                }
                return response;
            };
            let controller = client();
            assert.equal((await controller.start()).status, 'ready');
            loseBody = true;
            // Use the real, unchanged ten-second Study delivery budget.
            await assert.rejects(controller.answer('a6b7'), { name: 'TimeoutError' });
            assert.equal(controller.getState().canRetry, true);
            const accepted = persisted(database);
            assert.deepEqual(accepted.grades, ['again']);
            assert.equal(accepted.receipts, 1);
            const timedOut = deliveries.at(-1);
            assert.deepEqual(JSON.parse(saved).pending, JSON.parse(timedOut.body));

            await stop(host.child);
            host = await start(database, logs);
            controller = client();
            const replay = await controller.start();
            const replayDelivery = deliveries.at(-1);
            assert.equal(replayDelivery.body, timedOut.body);
            assert.notEqual(replayDelivery.id, timedOut.id);
            assert.notEqual(replayDelivery.run, timedOut.run);
            assert.equal(replay.revision, lostResult.revision);
            assert.equal(replay.attempt_id, lostResult.attempt_id);
            assert.deepEqual(replay.feedback, lostResult.feedback);
            assert.deepEqual(persisted(database), accepted);
            assert.equal(JSON.parse(saved).pending, null);

            const browser = diagnosticsSnapshot();
            const terminal = delivery => browser.events.find(event =>
                event.request_id === delivery.id && event.event === 'request_end');
            assert.equal(terminal(timedOut).phase, 'body');
            assert.equal(terminal(timedOut).outcome, 'timeout');
            assert.equal(terminal(timedOut).run_id, timedOut.run);
            assert.equal(terminal(replayDelivery).outcome, 'received');
            assert.equal(terminal(replayDelivery).server_request_matches, true);

            let events = [];
            const runtimeComplete = () => events.some(event => event.request_id === replayDelivery.id &&
                event.event === 'span.end' && event.operation === 'http.request');
            const deadline = performance.now() + 2000;
            while (performance.now() < deadline) {
                events = await logEvents(logs);
                if (runtimeComplete()) break;
                await new Promise(resolve => setTimeout(resolve, 20));
            }
            assert.ok(runtimeComplete(), 'Correlated runtime evidence did not arrive');
            for (const delivery of [timedOut, replayDelivery]) {
                const matched = events.filter(event => event.request_id === delivery.id);
                assert.ok(matched.some(event => event.event === 'http.response' && event.status_code === 200));
                assert.ok(matched.some(event => event.event === 'store.end'));
                assert.ok(matched.every(event => event.run_id === delivery.run));
            }
            const exported = JSON.stringify({ browser, events });
            for (const forbidden of ['Private review label', 'Private source', 'private-review-command',
                '2r3k1/5pp1/Q3b2p', '"a6b7"', '"a6c8"']) {
                assert.ok(!exported.includes(forbidden), `Diagnostic leak: ${forbidden}`);
            }
        } finally {
            globalThis.fetch = networkFetch;
            await stop(host?.child);
            await rm(dir, { recursive: true, force: true });
        }
    });
