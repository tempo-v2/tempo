// Run against the packaged candidate and the previous installed executable.
import assert from "node:assert/strict";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { request } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { databaseRows } from "./support/game_detection_readback.mjs";

const args = process.argv.slice(2);
assert.equal(args.length, 3, "Usage: node settings_browser.mjs OLD_BINARY CANDIDATE_BINARY CANDIDATE_UI");
const [previous, binary, ui] = args.map(path => resolve(path));
const { chromium, webkit } = await import(process.env.TEMPO_PLAYWRIGHT || "playwright");
const directory = await mkdtemp(join(tmpdir(), "tempo-settings-"));
const database = join(directory, "study.sqlite");
const endpoint = "/api/settings/study";
const phone = "https://phone-fixture.ts.net:8443";
const phoneHeaders = {
  Host: new URL(phone).host,
  "X-Forwarded-Host": new URL(phone).host,
  "X-Forwarded-Proto": "https",
  "Tailscale-User-Login": "owner@example.invalid",
  Origin: phone,
};
let child, chrome, safari, origin;

async function start() {
  child = spawn(binary, ["serve", database, ui, "0"], {
    env: { ...process.env, TEMPO_PHONE_ORIGIN: phone, TEMPO_PHONE_TAILSCALE_LOGIN: phoneHeaders["Tailscale-User-Login"] },
  });
  child.stderr.pipe(process.stderr);
  const stdout = createInterface({ input: child.stdout });
  const deadline = setTimeout(() => stdout.close(), 8000);
  try {
    for await (const line of stdout) {
      if (line.startsWith("Tempo study at ")) return line.slice("Tempo study at ".length);
    }
    throw new Error("Tempo did not start.");
  } finally {
    clearTimeout(deadline);
    stdout.close();
  }
}

async function stop() {
  if (child && child.exitCode === null && child.signalCode === null) {
    const exited = once(child, "exit");
    child.kill();
    await exited;
  }
}

function transfer(id, theme, fen, uci) {
  return {
    namespace: "settings-proof", source_id: id, solver_fen: fen, solution: [uci], label: id,
    provenance: ["tempo1:catalog-metadata:" + JSON.stringify({
      Motif: theme, Difficulty: "easy", DeckId: theme + "-easy-01", DeckPosition: 1,
    })],
  };
}

async function post(path, command) {
  const response = await fetch(origin + path, {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(command),
  });
  assert.equal(response.status, 200);
  return response.json();
}

function phoneRequest(route) {
  // Exercise the packaged Phone build and real ingress checks in a local
  // browser by supplying the headers normally added by the private proxy.
  return new Promise((resolve, reject) => {
    const req = request(route.request().url(), {
      method: route.request().method(), headers: { ...route.request().headers(), ...phoneHeaders },
    }, response => {
      const chunks = [];
      response.on("data", chunk => chunks.push(chunk));
      response.on("end", () => route.fulfill({
        status: response.statusCode, headers: response.headers, body: Buffer.concat(chunks),
      }).then(resolve, reject));
    });
    req.on("error", reject);
    req.end(route.request().postDataBuffer());
  });
}

const get = () => post(endpoint, { command: "get" });
const settingsButton = page => page.getByRole("button", { name: "Settings", exact: true });
const saveButton = page => page.getByRole("button", { name: "Save Study settings" });
const openingLimit = page => page.getByLabel("New opening moves per day");
const tacticLimit = page => page.getByLabel("New tactics per day");
const extraLimit = page => page.getByLabel("Extra tactics per request");
const savedNotice = page => page.getByText("Study settings saved.", { exact: true });
const studyCommand = (page, command) => page.waitForResponse(response =>
  response.url().endsWith("/api/study") && response.request().postDataJSON().command === command);
const forkFen = "4k3/8/8/8/8/8/4P3/4K3 w - - 0 1";
const pinFen = "4k3/8/8/8/8/8/3P4/4K3 w - - 0 1";

try {
  execFileSync(previous, ["init", database]);
  execFileSync(previous, ["configure", database, "0", "0", "2"]);
  const puzzles = join(directory, "puzzles.json");
  await writeFile(puzzles, JSON.stringify({
    schema_version: 1, puzzles: [
      transfer("FORK", "fork", forkFen, "e2e4"), transfer("PIN", "pin", pinFen, "d2d4"),
    ],
  }));
  execFileSync(previous, ["import-tactics", database, puzzles]);
  origin = await start();
  const first = await get();
  assert.deepEqual(first.introductions, { repertoire: 0, tactics: 0 });
  assert.equal(first.more_tactics, 2);
  assert.equal(first.selection, null);
  const input = { dataset_id: first.dataset_id, name: "Opening", learner_color: "white", pgn: "1. e4 *" };
  const repertoire = await post("/api/repertoires", { command: "preview", ...input });
  assert.equal((await post("/api/repertoires", { command: "save", ...input, token: repertoire.token })).status, "repertoires");
  const retired = spawnSync(binary, ["configure", database, "7", "8", "9"]);
  assert.equal(retired.status, 1);
  assert.match(retired.stderr.toString(), /Open Settings/);
  const initialProgress = await post("/api/study-progress", null);
  const oldStamp = { ...initialProgress.stamp };
  delete oldStamp.settings_revision;
  assert.deepEqual(await post("/api/study-progress", oldStamp), initialProgress);
  assert.equal(await post("/api/study-progress", initialProgress.stamp), null);

  chrome = await chromium.launch({ channel: "chrome", headless: true });
  const desktop = await chrome.newPage();
  desktop.setDefaultTimeout(8000);
  await desktop.addInitScript(() => {
    if (localStorage.getItem("tempo-v2-board-preferences") === null) {
      localStorage.setItem("tempo-v2-board-preferences", JSON.stringify({ theme: "green", pieceSet: "merida", volume: 0.25 }));
      localStorage.setItem("tempo-v2-board-sounds", "false");
    }
  });
  const stale = await chrome.newPage();
  await Promise.all([desktop.goto(origin), stale.goto(origin)]);
  await Promise.all([settingsButton(desktop).click(), settingsButton(stale).click()]);
  assert.equal(await openingLimit(stale).inputValue(), "0");
  assert.equal(await extraLimit(desktop).inputValue(), "2");
  assert.equal(await desktop.getByRole("radio", { name: "Automatic" }).isChecked(), true);
  assert.equal(await desktop.getByLabel("Board theme").inputValue(), "green");
  assert.equal(await desktop.getByLabel("Pieces").inputValue(), "merida");
  assert.equal(await desktop.getByLabel("Volume").inputValue(), "0.25");
  assert.equal(await desktop.getByLabel("Board sounds").isChecked(), false);
  await desktop.getByLabel("Board theme").selectOption("blue");
  await desktop.getByLabel("Volume").press("End");
  await desktop.getByLabel("Board sounds").check();
  assert.deepEqual(await desktop.evaluate(() => JSON.parse(localStorage.getItem("tempo-v2-board-preferences"))),
    { theme: "blue", pieceSet: "merida", volume: 1, soundEnabled: true });
  assert.equal(await desktop.evaluate(() => localStorage.getItem("tempo-v2-board-sounds")), null);

  const beforeSettings = databaseRows(database);
  await desktop.getByRole("radio", { name: "Selected themes" }).check();
  await desktop.getByRole("button", { name: "Clear selection" }).click();
  assert.equal(await saveButton(desktop).isEnabled(), false);
  await desktop.getByRole("checkbox", { name: "Forks", exact: true }).check();
  await extraLimit(desktop).fill("1");
  await desktop.route("**/api/settings/study", async route => {
    if (route.request().postDataJSON().command === "get") return route.continue();
    const response = await route.fetch();
    assert.equal((await response.json()).status, "study_settings");
    await route.abort();
  });
  await saveButton(desktop).click();
  await savedNotice(desktop).waitFor();
  await desktop.unroute("**/api/settings/study");
  assert.deepEqual(databaseRows(database).filter(([table]) => table !== "store_state"),
    beforeSettings.filter(([table]) => table !== "store_state"));
  const saved = await get();
  assert.deepEqual(saved.selection, { mode: "selected", theme_ids: ["fork"] });
  assert.equal(saved.more_tactics, 1);
  const invalid = {
    command: "save", dataset_id: saved.dataset_id, expected_revision: saved.revision,
    introductions: { repertoire: 0, tactics: 0 }, more_tactics: 1,
    selection: { mode: "selected", theme_ids: ["unknown_theme"] },
  };
  assert.equal((await post(endpoint, invalid)).code, "invalid_settings");
  assert.equal((await post(endpoint, { ...invalid, dataset_id: "other-dataset" })).code, "stale_dataset");
  assert.deepEqual(await get(), saved);

  await openingLimit(stale).fill("1");
  await saveButton(stale).click();
  const reload = stale.getByRole("button", { name: "Reload saved settings" });
  await reload.click();
  await reload.waitFor({ state: "detached" });
  assert.equal(await openingLimit(stale).inputValue(), "1");
  assert.equal(await extraLimit(stale).inputValue(), "1");
  assert.equal(await stale.getByRole("checkbox", { name: "Forks", exact: true }).isChecked(), true);

  await desktop.getByRole("button", { name: "Back" }).click();
  const grant = studyCommand(desktop, "learn_more_tactics");
  const issue = studyCommand(desktop, "next");
  await desktop.getByRole("button", { name: "Learn more tactics today" }).click();
  const firstGrant = await grant;
  const grantResult = await firstGrant.json();
  assert.equal(grantResult.tactics_remaining, 1);
  const active = await (await issue).json();
  assert.equal(active.display_fen, forkFen);
  await desktop.locator(".study-board cg-board").waitFor();
  const studyCalls = [];
  desktop.on("request", request => {
    if (request.url().endsWith("/api/study")) studyCalls.push(request.postDataJSON().command);
  });
  const withActive = databaseRows(database);
  await desktop.getByRole("button", { name: "Mute board" }).click();
  await settingsButton(desktop).click();
  assert.equal(await desktop.getByLabel("Board sounds").isChecked(), false);
  await desktop.getByRole("button", { name: "Back" }).click();
  await desktop.locator(".study-board cg-board").waitFor();
  assert.deepEqual(studyCalls, []);
  assert.deepEqual(databaseRows(database), withActive);
  await desktop.locator(".study-board .tempo-theme-blue.tempo-pieces-merida cg-board").waitFor();

  await saveButton(stale).click();
  await savedNotice(stale).waitFor();
  const progress = await post("/api/study-progress", initialProgress.stamp);
  assert.equal(progress.openings_limit, 1);
  assert.equal(progress.extra_tactics, 1);
  assert.deepEqual(await post("/api/study", firstGrant.request().postDataJSON()), grantResult);
  const skip = attempt => post("/api/study", {
    command: "skip", dataset_id: first.dataset_id, attempt_id: attempt.attempt_id,
    expected_revision: attempt.revision, submission_id: "skip-" + attempt.attempt_id,
  });
  assert.equal((await skip(active)).status, "abandoned");
  const summary = studyCommand(stale, "study_status");
  await stale.getByRole("button", { name: "Back" }).click();
  assert.equal((await (await summary).json()).done, false);
  const nextOpening = studyCommand(stale, "next");
  await stale.getByRole("button", { name: "Continue study" }).click();
  const opening = await (await nextOpening).json();
  assert.equal(opening.activity, "teach");
  assert.equal(opening.lesson.move_uci, "e2e4");

  await settingsButton(stale).click();
  await openingLimit(stale).fill("0");
  await tacticLimit(stale).fill("1");
  await extraLimit(stale).fill("0");
  await stale.getByRole("radio", { name: "Any tactic, including personal captures" }).check();
  await saveButton(stale).click();
  await savedNotice(stale).waitFor();
  assert.equal((await post("/api/study", { command: "next" })).attempt_id, opening.attempt_id);
  await skip(opening);
  const pin = await post("/api/study", { command: "next" });
  assert.equal(pin.display_fen, pinFen);

  safari = await webkit.launch();
  const mobile = await safari.newPage({ viewport: { width: 375, height: 600 }, isMobile: true, hasTouch: true });
  await mobile.route(origin + "/**", async route => {
    try { await phoneRequest(route); }
    catch { await route.abort().catch(() => {}); }
  });
  await mobile.goto(origin);
  await mobile.locator(".study-board cg-board").waitFor();
  await mobile.getByRole("region", { name: "Today's study progress" }).getByText(/\(\+1 today\)/).waitFor();
  await settingsButton(mobile).click();
  assert.equal(await mobile.getByLabel("Board theme").inputValue(), "brown");
  assert.equal(await openingLimit(mobile).count(), 0);
  await mobile.getByLabel("Board theme").selectOption("green");
  await mobile.getByRole("button", { name: "Back" }).click();
  await mobile.locator(".study-board .tempo-theme-green cg-board").waitFor();
  assert.equal(await desktop.evaluate(() => JSON.parse(localStorage.getItem("tempo-v2-board-preferences")).theme), "blue");
  await mobile.close();

  const current = await get();
  await stop();
  origin = await start();
  assert.deepEqual(await get(), current);
  const afterRestart = await post("/api/study", { command: "next" });
  assert.equal(afterRestart.attempt_id, pin.attempt_id);

  // An existing extra grant can push a configured base beyond the browser's
  // integer range. The projection stays exact and the original grant stays saved.
  const largest = await post(endpoint, {
    ...invalid, dataset_id: current.dataset_id, expected_revision: current.revision,
    introductions: { repertoire: 0, tactics: Number.MAX_SAFE_INTEGER }, selection: null,
    more_tactics: Number.MAX_SAFE_INTEGER,
  });
  assert.equal(largest.status, "study_settings");
  const capped = await post("/api/study-progress", progress.stamp);
  assert.equal(capped.tactics_limit, Number.MAX_SAFE_INTEGER);
  assert.equal(capped.extra_tactics, 1);
  assert.ok(Number.isSafeInteger((await post("/api/study", { command: "study_status" })).tactics_remaining));
  assert.deepEqual(await post("/api/study", firstGrant.request().postDataJSON()), grantResult);
  console.log("Verified Settings saves, conflict recovery, Study effects and phone/browser preferences.");
} finally {
  await Promise.all([chrome?.close(), safari?.close(), stop()]);
  await rm(directory, { recursive: true, force: true });
}
