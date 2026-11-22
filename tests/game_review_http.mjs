// Run with TEMPO_BINARY=/absolute/path/to/tempo node tests/game_review_http.mjs.
import assert from "node:assert/strict";
import { once } from "node:events";
import { execFileSync, spawn } from "node:child_process";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { DatabaseSync } from "node:sqlite";

const root = await mkdtemp(join(tmpdir(), "tempo-game-review-"));
const database = join(root, "study.sqlite");
const binary = resolve(process.env.TEMPO_BINARY || "target/debug/tempo");
let server;
let sql;
try {
  await mkdir(join(root, "ui/assets"), { recursive: true });
  await writeFile(join(root, "ui/index.html"), "<title>Game review fixture</title>");
  execFileSync(binary, ["init", database]);
  server = spawn(binary, ["serve", database, join(root, "ui"), "0"], { cwd: root, stdio: ["ignore", "pipe", "inherit"] });
  const [line] = await Promise.race([
    once(createInterface({ input: server.stdout }), "line"),
    once(server, "exit").then(([code]) => { throw Error("Tempo exited during startup: " + code); }),
  ]);
  const origin = line.replace("Tempo study at ", "");
  const post = async (route, data) => (await fetch(origin + "/api/" + route, {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(data),
  })).json();
  const history = async () => (await fetch(origin + "/api/games")).json();
  const { dataset_id } = await post("repertoires", { command: "list" });

  async function add(name, learner_color, pgn) {
    const input = { dataset_id, name, learner_color, pgn };
    const preview = await post("repertoires", { ...input, command: "preview" });
    const saved = await post("repertoires", { ...input, command: "save", token: preview.token });
    return saved.repertoires.find(book => book.name === name).id;
  }
  const italian = await add("My openings", "white", "1. e4 e5 2. Nf3 Nc6 3. Bc4 Nf6 4. d3 *");
  const other = await add("Different white plan", "white", "1. d4 d5 2. c4 *");
  const black = await add("Black", "black", "1. e4 c6 *");
  const teach = await post("study", { command: "next" });
  const learned = await post("study", {
    command: "acknowledge", dataset_id, attempt_id: teach.attempt_id,
    expected_revision: teach.revision, submission_id: "review-teach", answer: { uci: teach.lesson.move_uci },
  });
  assert.equal(learned.status, "complete");
  const moves = "1. e4 e5 2. Nf3 Nf6 3. Bc4 Nc6 4. d4 a6";
  const imported = await post("games/import", {
    dataset_id, command_id: "club", learner: "Andy",
    pgn: '[White "Andy"]\n[Black "Robin"]\n[Date "2026.11.17"]\n\n' + moves + " 1-0",
  });
  assert.equal(imported.new_games, 1);
  const [row] = (await history()).results;
  assert.equal(row.players.white, "Andy");
  assert.equal(row.players.black, "Robin");
  assert.equal(new Date(row.players.played_date_ms).toISOString().slice(0, 10), "2026-11-17");

  sql = new DatabaseSync(database, { readOnly: true, timeout: 5000 });
  function snapshot() {
    const tables = sql.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name").all();
    return tables.map(({ name }) => sql.prepare('SELECT * FROM "' + name + '"').all());
  }
  const address = { dataset_id, source: row.source, game_id: row.game_id, learner: row.learner, repertoire_id: italian };
  const review = (changes = {}) => post("games/review", { ...address, ...changes });
  const before = snapshot();
  const italianReview = await review();
  assert.equal(italianReview.status, "review");
  assert.equal(italianReview.positions.length, 9);
  assert.equal(italianReview.positions.at(-1).played_uci, null);
  assert.deepEqual(italianReview.comparison.first_departure, { kind: "opponent_gap", ply: 4 });
  assert.equal(italianReview.comparison.reentry_ply, 7);
  const transposition = italianReview.positions[6];
  assert.equal(transposition.book_state, "position");
  assert.deepEqual(transposition.prepared_uci, ["d2d3"]);
  assert.equal(transposition.played_uci, "d2d4");
  // Import can itself introduce a missed response through the accepted Game Again policy.
  const storedIntroduction = sql.prepare(
    "SELECT json_extract(l.state,'$.introduced') AS seen FROM current_content c JOIN learning l USING(target_id) WHERE c.repertoire=? AND c.position=?"
  ).get(italian, transposition.fen.split(" ").slice(0, 4).join(" "));
  assert.equal(transposition.introduced, Boolean(storedIntroduction.seen));
  assert.ok(italianReview.positions.some(position => position.introduced === false));
  assert.ok(italianReview.positions.some(position => position.introduced === true));
  const branch = await review({line:{from_ply:6,moves:["d2d3","a7a6","e1g1"]}});
  assert.deepEqual(branch.line.map(p => [p.uci,p.book_state,p.prepared_uci]), [
    ["d2d3","position",["d2d3"]], ["a7a6","endpoint",[]], ["e1g1","outside",[]],
  ]);
  assert.equal((await review({line:{from_ply:6,moves:["e1e8"]}})).code, "invalid_move");
  assert.equal((await review({line:{from_ply:99,moves:[]}})).code, "invalid_position");
  assert.deepEqual(transposition.other_route, ["e2e4", "e7e5", "g1f3", "b8c6", "f1c4", "g8f6"]);
  const switched = await review({ repertoire_id: other });
  assert.deepEqual(switched.positions[0].prepared_uci, ["d2d4"]);
  assert.equal(switched.comparison.first_departure.kind, "learner_move");
  const noBook = await review({ repertoire_id: null });
  assert.equal(noBook.comparison, null);
  assert.ok(noBook.positions.every(p => p.book_state === "unavailable" && p.introduced === null));
  for (const [changes, code] of [
    [{ repertoire_id: black }, "invalid_perspective"], [{ repertoire_id: "absent" }, "unknown_repertoire"],
    [{ dataset_id: "stale" }, "stale_dataset"], [{ game_id: "missing" }, "source_unavailable"],
    [{ learner: "missing" }, "invalid_perspective"],
  ]) assert.equal((await review(changes)).code, code);
  async function evidence(scope, request_id) {
    let reply = await post("analysis/evidence", {...scope, request_id});
    const deadline = Date.now() + 5000;
    while (reply.status === "running" && Date.now() < deadline) {
      await new Promise(resolve => setTimeout(resolve,10));
      reply = await post("analysis/result", {request_id});
    }
    return reply;
  }
  const scope = {source:"repertoire", fen:transposition.fen, dataset_id, repertoire_id:italian,
    review_version:{document_id:italianReview.document_id,revision:italianReview.revision}};
  const matched = await evidence(scope, "matched");
  assert.equal(matched.status,"complete");
  assert.deepEqual(matched.data.prepared_uci,transposition.prepared_uci);
  assert.equal(matched.data.dataset_id,dataset_id);
  assert.equal(matched.data.repertoire_id,italian);
  assert.equal(matched.data.document_id,italianReview.document_id);
  assert.equal(matched.data.content_revision,italianReview.revision);
  assert.equal((await evidence({...scope, repertoire_id:other},"another-book")).code,"stale_repertoire");
  assert.deepEqual(snapshot(), before);
  await post("repertoires", { command: "rename", dataset_id, repertoire_id: italian, name: "Renamed" });
  assert.deepEqual((await review()).positions, italianReview.positions);
  assert.equal((await evidence(scope,"old-revision")).code,"stale_repertoire");
  const edit = {dataset_id,repertoire_id:italian,name:"Renamed",learner_color:"white",pgn:"1. d4 d5 *"};
  const preview = await post("repertoires",{...edit, command:"preview"});
  await post("repertoires",{...edit, command:"save", token:preview.token});
  const changed = await review();
  assert.notEqual(changed.document_id,italianReview.document_id);
  const currentScope = {...scope,fen:changed.positions[0].fen,
    review_version:{revision:changed.revision,document_id:changed.document_id}};
  assert.deepEqual((await evidence(currentScope,"new-document")).data.prepared_uci,["d2d4"]);
  assert.equal((await evidence({...currentScope,review_version:{...currentScope.review_version,
    document_id:italianReview.document_id}},"old-document")).code,"stale_repertoire");
  // Identity checks must not decode unrelated Study state.
  const active = await post("study", { command: "next" });
  assert.ok(active.attempt_id);
  const write = new DatabaseSync(database, { timeout: 5000 });
  try {
    assert.equal(write.prepare("UPDATE attempts SET record='{}' WHERE id=?").run(active.attempt_id).changes, 1);
    assert.equal(write.prepare(
      "UPDATE learning SET state='{}' WHERE target_id=(SELECT target_id FROM attempts WHERE id=?)"
    ).run(active.attempt_id).changes, 1);
    assert.equal(write.prepare(`
      INSERT INTO repertoire_run_steps(
        dataset_id, ordinal, repertoire_id, source_id, target_id, version, mode, lead_in, skipped_warmup
      )
      SELECT h.dataset_id, 0, d.repertoire_id, d.source_id, h.target_id, h.version, '"test"', '?', 0
      FROM content_history h
      JOIN current_content c ON c.target_id=h.target_id AND c.dataset_id=h.dataset_id
      JOIN repertoire_documents d ON d.repertoire_id=c.repertoire AND d.dataset_id=c.dataset_id
      WHERE d.repertoire_id=?
      LIMIT 1
    `).run(italian).changes, 1);
    write.exec("UPDATE store_state SET policy='{}' WHERE singleton=1");
    assert.equal((await review({ repertoire_id: null })).status, "review");
    assert.equal((await review({ dataset_id: "stale" })).code, "stale_dataset");
  } finally {
    write.close();
  }
  console.log("PASS: Games identity survives unreadable Study records and rejects a stale dataset.");
  console.log("PASS: named history, all game boards, exact transposition, distinct destination, learning facts, stale addresses, name invariance and zero database changes across reads.");
} finally {
  sql?.close();
  if (server && server.exitCode === null) {
    const ended = once(server, "exit");
    server.kill();
    await ended;
  }
  await rm(root, { recursive: true, force: true });
}
