// Run with the built production binary and public external fixture example.
import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
import { DatabaseSync } from 'node:sqlite';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const target = process.env.CARGO_TARGET_DIR
    ?? fileURLToPath(new URL('../../target', import.meta.url));
const tempo = join(target, 'debug', 'tempo');
const fixture = join(target, 'debug', 'examples', 'game_detection_contract_host');

async function start(binary, args) {
    const child = spawn(binary, args);
    const output = createInterface({ input: child.stdout });
    let diagnostics = '';
    child.stderr.on('data', chunk => { diagnostics += chunk; });
    for await (const line of output) {
        const match = /^Tempo study at (http:\/\/127\.0\.0\.1:\d+)$/.exec(line);
        if (match) return {
            url: match[1],
            async stop() {
                if (child.exitCode !== null || child.signalCode !== null) return;
                const done = once(child, 'exit');
                child.kill();
                await done;
            },
        };
    }
    assert.fail(diagnostics || 'Tempo did not start');
}

async function request(host, route, body) {
    const response = await fetch(host.url + '/api/' + route, body === undefined ? {} : {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
    });
    return { status: response.status, body: await response.json() };
}

function databaseRows(database) {
    const db = new DatabaseSync(database, { readOnly: true });
    try {
        return db.prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .all().map(({ name }) => [name, db.prepare('SELECT * FROM "' + name.replaceAll('"', '""') + '"').all()]);
    } finally {
        db.close();
    }
}

test('external fixture scans an accepted Black-start game through the daily host without durable effects',
    { timeout: 15000 }, async t => {
        const directory = await mkdtemp(join(tmpdir(), 'tempo-detection-'));
        let host;
        t.after(async () => {
            await host?.stop();
            await rm(directory, { recursive: true, force: true });
        });
        const database = join(directory, 'study.sqlite');
        const ui = join(directory, 'ui');
        await mkdir(join(ui, 'assets'), { recursive: true });
        await writeFile(join(ui, 'index.html'), '<!doctype html><title>HTTP contract</title>');
        execFileSync(tempo, ['init', database]);
        host = await start(tempo, ['serve', database, ui, '0']);
        assert.deepEqual(await request(host, 'games/detectors'), {
            status: 200,
            body: { schema_version: 1, status: 'unconfigured', detectors: [] },
        });
        const db = new DatabaseSync(database, { readOnly: true });
        const { dataset_id } = db.prepare('SELECT dataset_id FROM store_state').get();
        const imported = await request(host, 'games/import', {
            dataset_id, command_id: 'test-game', learner: 'Learner',
            pgn: '[White "Opponent"]\n[Black "Learner"]\n[Result "0-1"]\n[SetUp "1"]\n'
                + '[FEN "4k3/4p3/8/8/8/8/4P3/4K3 b - - 7 12"]\n12...e5 13.e4 0-1\n',
        });
        assert.equal(imported.body.new_games, 1, JSON.stringify(imported.body));
        const { source, id: game_id } = db.prepare('SELECT source, id FROM games').get();
        db.close();
        await host.stop();
        const intent = {
            schema_version: 1, request_id: 'exact-A', dataset_id, source, game_id,
            learner: 'Learner', detector_id: 'contract-fixture',
        };
        async function finish(query) {
            for (;;) {
                const reply = await request(host, 'games/detection-result', {
                    schema_version: 1, request_id: query.request_id, dataset_id,
                });
                if (reply.status !== 202) return reply;
                await new Promise(resolve => setTimeout(resolve, 10));
            }
        }
        let initialKey;
        for (const [scenario, status] of [
            ['valid', 'complete'], ['empty', 'complete'], ['unsupported', 'unsupported'],
            ['invalid', 'invalid_result'], ['failed', 'failed'], ['slow', 'timed_out'],
        ]) {
            host = await start(fixture, [database, ui, '0', scenario]);
            const before = databaseRows(database);
            const discovery = await request(host, 'games/detectors');
            assert.equal(discovery.body.status, 'available');
            assert.equal(discovery.body.detectors[0].configuration_fingerprint, scenario);
            assert.equal((await request(host, 'games/detection-result', {
                schema_version: 1, request_id: intent.request_id, dataset_id,
            })).body.code, 'expired');
            const admitted = await request(host, 'games/detection', intent);
            assert.equal(admitted.status, 202, JSON.stringify(admitted.body));
            if (scenario === 'slow') {
                const study = await request(host, 'study-progress', null);
                assert.equal(study.status, 200);
                assert.notEqual(study.body.code, 'unsupported');
                const busy = await request(host, 'games/detection', { ...intent, request_id: 'during-A' });
                assert.equal(busy.body.code, 'busy');
            }
            const terminal = await finish(intent);
            assert.equal(terminal.status, 200);
            assert.equal(terminal.body.status, status, JSON.stringify(terminal.body));
            assert.equal(terminal.body.game_id, game_id);
            if (scenario === 'valid') {
                initialKey = terminal.body.analysis_key;
                assert.equal(terminal.body.learner_color, 'black');
                assert.equal(terminal.body.findings[0].occurrence, 0);
                assert.equal(terminal.body.findings[0].candidate_move.uci, 'e7e5');
                assert.match(terminal.body.findings[0].fen, /b - - 7 12$/);
                const white = { ...intent, request_id: 'white', learner: 'Opponent' };
                assert.equal((await request(host, 'games/detection', white)).status, 202);
                const whiteResult = await finish(white);
                assert.equal(whiteResult.body.learner_color, 'white');
                assert.notEqual(whiteResult.body.input_key, terminal.body.input_key);
                assert.equal(whiteResult.body.findings[0].occurrence, 1);
            }
            if (scenario === 'empty') {
                assert.deepEqual(terminal.body.findings, []);
                assert.notEqual(terminal.body.analysis_key, initialKey);
            }
            assert.deepEqual(await request(host, 'games/detection', intent), terminal);
            assert.equal((await request(host, 'games/detection', { ...intent, learner: 'Opponent' })).body.code, 'conflict');
            assert.deepEqual(databaseRows(database), before);
            await host.stop();
        }
    });
