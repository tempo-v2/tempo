// Run after building the browser artifacts and the Tempo binary.
import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { DatabaseSync } from 'node:sqlite';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { createStudyController } from '../ui/study_controller.js';

const binary = process.env.TEMPO_BINARY ?? fileURLToPath(new URL('../../target/debug/tempo', import.meta.url));
const ui = fileURLToPath(new URL('../ui/dist', import.meta.url));

// Solver-start state of Lichess 03ySM after b6d4, from Tempo mateIn2-focused-04.
const tactic = {
    namespace: 'tempo-pack-test',
    source_id: '03ySM',
    solver_fen: '2r3k1/5pp1/Q3b2p/3p4/3q4/3B3P/1P3PP1/4R1K1 w - - 0 27',
    solution: ['a6c8', 'e6c8', 'e1e8'],
    label: 'Mate in two',
    provenance: ['mateIn2-focused-04 (Lichess CC0)'],
};

async function start(database) {
    const child = spawn(binary, ['serve', database, ui, '0']);
    const output = readline.createInterface({ input: child.stdout });
    let diagnostics = '';
    child.stderr.on('data', chunk => { diagnostics += chunk; });
    let url;
    for await (const line of output) {
        const match = line.match(/^Tempo tactics: (http:\/\/127\.0\.0\.1:\d+)$/);
        if (match) {
            url = match[1];
            break;
        }
    }
    assert.ok(url, diagnostics || 'Tempo server did not start');
    return {
        url,
        async stop() {
            if (child.exitCode !== null || child.signalCode !== null) return;
            const exited = once(child, 'exit');
            child.kill();
            await exited;
        },
    };
}

function persisted(database) {
    const db = new DatabaseSync(database, { readOnly: true });
    try {
        return {
            grades: db.prepare("SELECT json_extract(record, '$.input.grade') AS grade FROM assessments ORDER BY ordinal")
                .all().map(row => row.grade),
            receipts: db.prepare('SELECT count(*) AS total FROM receipts').get().total,
            attempts: db.prepare('SELECT count(*) AS total FROM attempts').get().total,
            state: db.prepare('SELECT hex(revision) AS revision, dataset_id FROM store_state').get(),
        };
    } finally {
        db.close();
    }
}

test('HTTP and SQLite retain the exact committed move across a lost reply, restart and client reconstruction', { timeout: 15_000 }, async t => {
    const dir = await mkdtemp(path.join(tmpdir(), 'tempo-v2-http-'));
    let host;
    t.after(async () => {
        await host?.stop();
        await rm(dir, { recursive: true, force: true });
    });
    const database = path.join(dir, 'practice.sqlite');
    const transfer = path.join(dir, 'normalized.json');
    await writeFile(transfer, JSON.stringify({ schema_version: 1, puzzles: [tactic] }));
    execFileSync(binary, ['init-practice', database]);
    execFileSync(binary, ['import-tactics', database, transfer]);
    host = await start(database);
    const html = await fetch(host.url);
    assert.equal(html.status, 200);
    assert.match(await html.text(), /assets\/app\.js/);
    for (const resource of ['/assets/app.js', '/assets/app.css']) {
        const response = await fetch(host.url + resource);
        assert.equal(response.status, 200);
        assert.ok((await response.arrayBuffer()).byteLength > 0);
    }
    for (const resource of ['/normalized.json', '/assets/normalized.json', '/practice.sqlite']) {
        assert.equal((await fetch(host.url + resource)).status, 404);
    }

    let serial = 0;
    let stored = null;
    let loseMove = false;
    let lostResult;
    const sent = [];
    function controller() {
        return createStudyController({
            transport: { async send(command) {
                sent.push(structuredClone(command));
                const response = await fetch(host.url + '/api/study', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(command),
                });
                assert.equal(response.status, 200);
                const view = await response.json();
                if (loseMove) {
                    loseMove = false;
                    lostResult = view;
                    throw new Error('Reply discarded after durable commit');
                }
                return view;
            } },
            storage: {
                load: () => stored && JSON.parse(stored),
                save: envelope => { stored = JSON.stringify(envelope); },
            },
            render() {},
            makeSubmissionId: () => 'test-' + ++serial,
        });
    }
    let client = controller();
    const issued = await client.start();
    assert.equal(issued.status, 'ready');
    assert.equal(issued.display_fen, issued.prompt.fen);
    assert.ok(issued.legal_moves.includes('a6b7'));
    assert.ok(!JSON.stringify(issued).includes('"expected"'));

    const before = persisted(database);
    const illegal = await client.answer('a6a6');
    assert.equal(illegal.code, 'invalid_move');
    assert.equal(illegal.retryable, false);
    assert.equal(client.getState().canRetry, false);
    assert.deepEqual(persisted(database), before);

    loseMove = true;
    await assert.rejects(client.answer('a6b7'), /discarded after durable commit/);
    assert.equal(client.getState().canRetry, true);
    const savedCommand = sent.at(-1);
    assert.deepEqual(JSON.parse(stored).pending, savedCommand);
    const lost = persisted(database);
    assert.deepEqual(lost.grades, ['again']);
    assert.equal(lost.receipts, 1);

    await host.stop();
    host = await start(database);
    client = controller();
    const replay = await client.start();
    assert.deepEqual(sent.at(-1), savedCommand);
    assert.equal(replay.attempt_id, lostResult.attempt_id);
    assert.equal(replay.revision, lostResult.revision);
    assert.deepEqual(replay.feedback, lostResult.feedback);
    assert.equal(replay.display_fen, issued.display_fen);
    assert.deepEqual(persisted(database), lost);
    assert.equal(JSON.parse(stored).pending, null);

    const middle = await client.answer('a6c8');
    assert.equal(middle.status, 'ready');
    const complete = await client.answer('e1e8');
    assert.equal(complete.status, 'complete');
    assert.equal(complete.assessment, 'again');
    assert.deepEqual(persisted(database).grades, ['again']);
    assert.deepEqual(complete.legal_moves, []);
    assert.notEqual(complete.display_fen, middle.display_fen);

    client = controller();
    const reloaded = await client.start();
    assert.equal(reloaded.attempt_id, complete.attempt_id);
    assert.equal(reloaded.display_fen, complete.display_fen);
    const repeat = await client.next();
    assert.notEqual(repeat.attempt_id, issued.attempt_id);
    assert.equal(repeat.display_fen, issued.display_fen);
    await client.answer('a6c8');
    assert.equal((await client.answer('e1e8')).assessment, 'good');
    assert.deepEqual(persisted(database).grades, ['again', 'good']);
    assert.equal((await client.next()).status, 'empty');
});
