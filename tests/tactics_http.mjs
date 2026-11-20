// Run after building the browser artifacts and the Tempo binary.
import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { once } from 'node:events';
import { connect } from 'node:net';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { DatabaseSync } from 'node:sqlite';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { createStudyController } from '../../tempo-ui/study_controller.js';
import { databaseRows } from './support/game_detection_readback.mjs';

const binary = process.env.TEMPO_BINARY ?? fileURLToPath(new URL('../../target/debug/tempo', import.meta.url));
const ui = fileURLToPath(new URL('../../tempo-ui/dist', import.meta.url));

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
        const match = line.match(/^Tempo study at (http:\/\/127\.0\.0\.1:\d+)$/);
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
    for (const clip of ['move-self', 'move-opponent', 'capture', 'check', 'castle', 'promote', 'illegal']) {
        const response = await fetch(`${host.url}/assets/${clip}.mp3`);
        assert.equal(response.status, 200, clip);
        assert.equal(response.headers.get('content-type'), 'audio/mpeg', clip);
        const original = await readFile(new URL(`../../tempo-ui/sounds/${clip}.mp3`, import.meta.url));
        assert.deepEqual(Buffer.from(await response.arrayBuffer()), original, clip);
    }
    for (const resource of ['/normalized.json', '/assets/normalized.json', '/practice.sqlite']) {
        assert.equal((await fetch(host.url + resource)).status, 404);
    }

    let serial = 0;
    let stored = null;
    let loseMove = false;
    let interruptNext = false;
    let lostResult;
    const sent = [];
    function controller() {
        return createStudyController({
            transport: { async send(command) {
                sent.push(structuredClone(command));
                if (interruptNext && command.command === 'next') {
                    interruptNext = false;
                    throw new Error('Next interrupted before delivery');
                }
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
    assert.equal(issued.feedback, null);
    assert.ok(!JSON.stringify(issued).includes('correction_uci'));

    const post = async (route, command) => {
        const response = await fetch(`${host.url}/api/${route}`, {
            method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(command),
        });
        assert.equal(response.status, 200);
        return response.json();
    };
    const before = persisted(database);
    const page = await post('tactics-collection', { command: 'list', search: 'Lichess CC0' });
    assert.equal(page.dataset_id, issued.dataset_id);
    assert.deepEqual(page.rows, [{ target: issued.target_id, label: tactic.label, introduced: false, due: false }]);
    const detail = { command: 'detail', dataset_id: page.dataset_id, target: issued.target_id, show_solution: true };
    const blocked = await post('tactics-collection', detail);
    assert.equal(blocked.solution_blocked, true);
    assert.ok(!Object.hasOwn(blocked, 'solution'));
    assert.equal(blocked.fen, issued.display_fen);
    const progress = await post('study-progress', null);
    assert.equal(progress.tactics_introduced, 0);
    assert.equal(await post('study-progress', progress.stamp), null);
    assert.deepEqual(persisted(database), before);
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
    assert.equal(replay.feedback.correction_uci, 'a6c8');
    assert.equal(replay.display_fen, issued.display_fen);
    assert.deepEqual(persisted(database), lost);
    assert.equal(JSON.parse(stored).pending, null);

    const correction = await post('study-progress', progress.stamp);
    assert.equal(correction.active_correction, true);
    assert.equal(correction.tactics_introduced, 1);
    const middle = await client.answer('a6c8');
    const middleCommand = sent.at(-1);
    assert.equal(middle.status, 'ready');
    assert.ok(!Object.hasOwn(middle.feedback, 'correction_uci'));
    assert.equal(middle.history.moves.length, 2);
    assert.match(middle.history.moves[0].notation, /Qxc8/);
    assert.match(middle.history.moves[1].notation, /Bxc8/);
    assert.ok(!JSON.stringify(middle.history).includes('Re8'));
    const complete = await client.answer('e1e8');
    assert.equal(complete.status, 'complete');
    assert.equal(complete.assessment, 'again');
    assert.deepEqual(persisted(database).grades, ['again']);
    assert.deepEqual(complete.legal_moves, []);
    assert.notEqual(complete.display_fen, middle.display_fen);
    assert.equal(complete.history.moves.length, 3);
    const after = persisted(database);
    assert.deepEqual((await post('study', middleCommand)).history, middle.history);
    const metadata = await post('tactics-collection', { ...detail, show_solution: false });
    assert.ok(!Object.hasOwn(metadata, 'solution'));
    const inspected = await post('tactics-collection', detail);
    assert.equal(inspected.solution_blocked, false);
    assert.equal(inspected.solution.moves.length, 3);
    assert.deepEqual(persisted(database), after);

    interruptNext = true;
    const continuation = sent.length;
    await assert.rejects(client.next(), /Next interrupted before delivery/);
    assert.equal(client.getState().retryingNext, true);
    assert.equal(client.getState().view.status, 'complete');
    assert.deepEqual(persisted(database), after);
    const reloaded = await client.retry();
    assert.deepEqual(sent.slice(continuation), [{ command: 'next' }, { command: 'next' }]);
    assert.equal(reloaded.status, 'empty');
    assert.equal(reloaded.done, true);
    assert.equal(reloaded.can_practice_tactics, true);
    const schedulingRows = () => databaseRows(database).filter(([name]) =>
        name === 'learning' || name === 'daily_tactics_allowances');
    const beforePractice = schedulingRows();
    const practice = await client.practiceTactics();
    assert.equal(practice.activity, 'practice');
    assert.notEqual(practice.attempt_id, issued.attempt_id);
    assert.equal(practice.display_fen, issued.display_fen);
    await client.answer('a6c8');
    assert.equal((await client.answer('e1e8')).assessment, 'good');
    assert.deepEqual(persisted(database).grades, ['again']);
    assert.deepEqual(schedulingRows(), beforePractice);
    assert.equal((await client.next()).status, 'empty');
});

test('SQLite rolls back a teaching acknowledgement if its receipt cannot be stored', { timeout: 15_000 }, async t => {
    const dir = await mkdtemp(path.join(tmpdir(), 'tempo-v2-rollback-'));
    let host;
    t.after(async () => {
        await host?.stop();
        await rm(dir, { recursive: true, force: true });
    });
    const database = path.join(dir, 'opening.sqlite');
    execFileSync(binary, ['init', database]);
    host = await start(database);
    const post = async (url, command) => {
        const result = await fetch(host.url + '/api/' + url, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(command),
        });
        return result.json();
    };
    const { dataset_id } = await post('repertoires', { command: 'list' });
    const input = { dataset_id, name: 'Opening', learner_color: 'white', pgn: '1. e4 *' };
    const preview = await post('repertoires', { command: 'preview', ...input });
    const save = await post('repertoires', { command: 'save', ...input, token: preview.token });
    assert.equal(save.status, 'repertoires');
    const card = await post('study', { command: 'next' });
    const ack = {
        command: 'acknowledge', dataset_id, submission_id: 'teaching-rollback',
        attempt_id: card.attempt_id, expected_revision: card.revision, answer: { uci: 'e2e4' },
    };
    const before = persisted(database);
    const wrongShown = await post('study', {
        ...ack, submission_id: 'wrong-shown-move', answer: { uci: 'd2d4' },
    });
    assert.equal(wrongShown.status, 'error');
    assert.equal(wrongShown.code, 'try_shown_move');
    assert.deepEqual(persisted(database), before);
    assert.deepEqual(await post('study', { command: 'next' }), card);
    const sql = new DatabaseSync(database);
    try {
        sql.exec(`CREATE TRIGGER fail_test_receipt BEFORE INSERT ON receipts
            BEGIN SELECT RAISE(ABORT, 'disposable receipt failure'); END;`);
        const failure = await post('study', ack);
        assert.equal(failure.status, 'error');
        assert.equal(failure.retryable, true);
        assert.deepEqual(persisted(database), before);
        assert.deepEqual(await post('study', { command: 'next' }), card);
        assert.equal(sql.prepare("SELECT json_extract(state, '$.taught_at') AS taught FROM learning").get().taught, null);
        sql.exec('DROP TRIGGER fail_test_receipt');
        const accepted = await post('study', ack);
        assert.equal(accepted.assessment, null);
        assert.equal(accepted.status, 'complete');
        assert.deepEqual(await post('study', ack), accepted);
        assert.equal(persisted(database).receipts, 1);
        assert.deepEqual(persisted(database).grades, []);
    } finally {
        sql.close();
    }
});

test('large PGN commands reach repertoire, tactic, and Games consumers', { timeout: 30_000 }, async t => {
    const dir = await mkdtemp(path.join(tmpdir(), 'tempo-pgn-upload-'));
    const database = path.join(dir, 'upload.sqlite');
    execFileSync(binary, ['init', database]);
    const host = await start(database);
    t.after(async () => {
        await host.stop();
        await rm(dir, { recursive: true, force: true });
    });
    const post = async (route, command) => {
        const response = await fetch(host.url + '/api/' + route, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(command),
        });
        return response.json();
    };
    const address = new URL(host.url);
    const socket = connect(Number(address.port), address.hostname);
    socket.setTimeout(5_000, () => socket.destroy(new Error('Timed out waiting for HTTP reply')));
    socket.end([
        'POST /api/repertoires HTTP/1.1',
        'Host: ' + address.host,
        'Content-Type: application/json',
        'Content-Length: 2000',
        'Connection: close',
        '',
        '{"command":"list"}',
    ].join('\r\n'));
    let truncatedReply = '';
    for await (const chunk of socket) truncatedReply += chunk;
    assert.match(truncatedReply, /Could not read the command/);

    const { dataset_id } = await post('repertoires', { command: 'list' });
    const padding = '[Site "' + 'a'.repeat(2 * 1024 * 1024) + '"]\n';
    const input = { dataset_id, name: 'Large', learner_color: 'white', pgn: padding + '1.e4 e5 *' };
    const preview = await post('repertoires', { command: 'preview', ...input });
    assert.equal(preview.status, 'preview', JSON.stringify(preview));
    const saved = await post('repertoires', { command: 'save', ...input, token: preview.token });
    assert.equal(saved.status, 'repertoires', JSON.stringify(saved));

    const repeated = 'Nf3 Nf6 Ng1 Ng8 '.repeat(128) + '*\n';
    const tactics = await post('tactics', {
        command: 'preview', dataset_id, pgn: padding + repeated + repeated.repeat(32),
    });
    assert.equal(tactics.status, 'tactics_preview', tactics.message);
    assert.equal(tactics.rows.length, 33);
    assert.ok(tactics.rows.every(row => row.status === 'selection_required' && row.routes[0].moves.length === 512));

    const capture = {
        dataset_id, pgn: padding + '[SetUp "1"]\n[FEN "4k3/8/8/8/8/8/4P3/4K3 w - - 0 1"]\n1.e4 *',
    };
    const capturePreview = await post('tactics', { command: 'preview', ...capture });
    assert.equal(capturePreview.rows[0].status, 'valid', JSON.stringify(capturePreview));
    const captureSave = await post('tactics', {
        command: 'save', ...capture, token: capturePreview.token, selected: [0],
    });
    assert.equal(captureSave.status, 'tactics_saved', JSON.stringify(captureSave));

    const game = '[White "Player"]\n[Black "Opponent"]\n[Result "1-0"]\n1.e4 e5 1-0\n\n';
    const games = await post('games/import', {
        command_id: 'large-pgn', dataset_id, learner: 'Player', pgn: padding + game.repeat(501),
    });
    assert.equal(games.fetched, 501, JSON.stringify(games));
    assert.equal(games.errors.length, 0, JSON.stringify(games.errors));
    assert.equal(games.new_games + games.duplicates, 501);
});
