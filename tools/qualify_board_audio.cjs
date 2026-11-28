// Run after npm --prefix ui run build with the tools dependencies installed.
const assert = require("node:assert/strict");
const { readFile } = require("node:fs/promises");
const path = require("node:path");
const { chromium } = require("playwright-core");
const { Chess } = require("../ui/node_modules/chess.js");

const root = path.join(__dirname, "../ui/dist");
const cues = ["move-self", "move-opponent", "capture", "check", "castle", "promote", "illegal"];
const mime = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".mp3": "audio/mpeg", ".svg": "image/svg+xml" };
const board = new Chess();
const fen = board.fen();
const legal = () => board.moves({ verbose: true }).map(move => move.lan);
const opening = {
  dataset: "audio", attempt: 1, kind: "opening", activity_mode: "single_answer",
  color: "white", fen, legal: legal(), title: "Opening", result: null,
};
const puzzle = {
  dataset: opening.dataset, play: 1, puzzle: "audio-line", step: 0, mode: "study",
  color: "white", fen, legal: legal(), last_move: null, result: null, solution: null,
};
board.move("e4");
board.move("e5");
const afterReply = { ...puzzle, step: 2, fen: board.fen(), legal: legal(), last_move: "e7e5", before_fen: fen };
board.move("Nf3");
const solved = {
  ...afterReply, step: 3, fen: board.fen(), legal: [], last_move: "g1f3", before_fen: afterReply.fen,
  result: { rating: "Good", repeat: "None", due_day: null }, solution: { fen, moves: ["e2e4", "e7e5", "g1f3"] },
};

async function move(page, from, to) {
  const { x, y, width, height } = await page.locator("cg-board").boundingBox();
  for (const square of [from, to]) {
    await page.mouse.click(x + (square.charCodeAt(0) - 97 + 0.5) * width / 8, y + (8.5 - Number(square[1])) * height / 8);
  }
}

async function heard(page, expected) {
  await page.waitForFunction((count) => window.boardAudio.started.length >= count, expected.length);
  assert.deepEqual(await page.evaluate(() => window.boardAudio.started), expected);
}

async function qualify() {
  const browser = await chromium.launch({
    args: process.argv.includes("--single-process") ? ["--single-process", "--disable-gpu"] : [],
  });
  try {
    const page = await browser.newPage();
    const faults = [];
    page.on("pageerror", error => faults.push(error.message));
    await page.addInitScript(() => {
      const bytes = new WeakMap();
      const clips = new WeakMap();
      const observations = window.boardAudio = { decoded: [], started: [], stopped: [] };
      window.addEventListener("pointerup", () => { observations.lastInput = performance.now(); }, true);
      const fetchResource = window.fetch;
      window.fetch = async (...args) => {
        const response = await fetchResource(...args);
        const cue = response.url.match(/\/([^/]+)\.mp3$/)?.[1];
        if (cue) {
          const unpack = response.arrayBuffer.bind(response);
          response.arrayBuffer = async () => {
            const buffer = await unpack();
            bytes.set(buffer, cue);
            return buffer;
          };
        }
        return response;
      };
      const decode = AudioContext.prototype.decodeAudioData;
      AudioContext.prototype.decodeAudioData = async function (buffer) {
        const clip = await decode.call(this, buffer);
        const cue = bytes.get(buffer);
        clips.set(clip, cue);
        observations.decoded.push(cue);
        return clip;
      };
      const start = AudioBufferSourceNode.prototype.start;
      AudioBufferSourceNode.prototype.start = function (...args) {
        start.apply(this, args);
        observations.started.push(clips.get(this.buffer));
        observations.lastStart = performance.now();
      };
      const stop = AudioBufferSourceNode.prototype.stop;
      AudioBufferSourceNode.prototype.stop = function (...args) {
        stop.apply(this, args);
        observations.stopped.push(clips.get(this.buffer));
      };
    });
    let card = opening;
    let cardHeld;
    let answers = 0;
    let responseHeld = Promise.withResolvers();
    await page.route("http://tempo.test/**", async route => {
      const { pathname } = new URL(route.request().url());
      if (pathname === "/api/study") {
        await cardHeld?.promise;
        return route.fulfill({ json: card });
      }
      if (pathname === "/api/study/answer") {
        if (++answers === 1) {
          await responseHeld.promise;
          return route.abort();
        }
        await responseHeld.promise;
        const { played } = route.request().postDataJSON();
        return route.fulfill({ json: { rating: "Good", answers: [played], repeat_after: null } });
      }
      if (pathname === "/api/study/next") return route.fulfill({
        json: { ...card, attempt: 3, lead_in: undefined, introduction_move: "g1f3" },
      });
      if (pathname === "/api/study/puzzle-move") {
        const { step } = route.request().postDataJSON();
        if (step === 0) await responseHeld.promise;
        return route.fulfill({ json: step === 0 ? afterReply : solved });
      }
      if (pathname === "/api/tactics") {
        return route.fulfill({ json: [{ id: "audio-line", name: "Audio replay", route: solved.solution }] });
      }
      if (pathname === "/api/tactics/plays") return route.fulfill({ json: [] });
      if (pathname === "/api/tactics/packs") return route.fulfill({ json: { themes: [] } });
      if (pathname === "/api/openings") {
        return route.fulfill({ json: [{ id: "saved-opening", name: "Saved audio line", color: "white", route: solved.solution }] });
      }
      if (pathname.startsWith("/api/")) return route.fulfill({ json: null });
      const file = pathname === "/" ? "index.html" : pathname.slice(1);
      return route.fulfill({ contentType: mime[path.extname(file)], body: await readFile(path.join(root, file)) });
    });
    const prime = async () => {
      await page.getByRole("heading", { name: "Study" }).click();
      await page.waitForFunction(count => window.boardAudio.decoded.length === count, cues.length);
      assert.deepEqual((await page.evaluate(() => window.boardAudio.decoded)).sort(), [...cues].sort());
    };

    await page.goto("http://tempo.test");
    await prime();
    await page.getByRole("heading", { name: "White to move" }).waitFor();
    await move(page, "e2", "e4");
    await heard(page, ["move-self"]);
    const studyDelay = await page.evaluate(() => window.boardAudio.lastStart - window.boardAudio.lastInput);
    assert.equal(await page.getByText("Good", { exact: true }).count(), 0);
    responseHeld.resolve();
    await page.getByRole("button", { name: "Retry" }).waitFor();
    await page.getByRole("button", { name: "Retry" }).click();
    await page.getByText("Good", { exact: true }).waitFor();
    await heard(page, ["move-self"]);
    assert.equal(answers, 2);

    card = { ...opening, kind: "tactic", activity_mode: "full_line", puzzle };
    responseHeld = Promise.withResolvers();
    await page.reload();
    await prime();
    await page.getByRole("button", { name: "Reveal solution" }).waitFor();
    await move(page, "e2", "e4");
    await heard(page, ["move-self"]);
    const puzzleDelay = await page.evaluate(() => window.boardAudio.lastStart - window.boardAudio.lastInput);
    await page.waitForFunction(() => [...document.querySelectorAll("cg-board piece")]
      .some(piece => piece.cgKey === "e4" && piece.cgPiece === "white pawn"));
    responseHeld.resolve();
    await heard(page, ["move-self", "move-opponent"]);
    await move(page, "g1", "f3");
    await page.getByText("Solved", { exact: true }).waitFor();
    await heard(page, ["move-self", "move-opponent", "move-self"]);

    await page.getByRole("button", { name: "Show solution" }).click();
    await page.getByRole("button", { name: "First position" }).click();
    const next = page.getByRole("button", { name: "Next move", exact: true });
    await next.click();
    await next.click();
    await next.click();
    await heard(page, ["move-self", "move-opponent", "move-self", "move-self", "move-opponent", "move-self"]);
    await page.getByRole("button", { name: "Settings" }).click();
    await page.getByRole("checkbox", { name: "Board sounds" }).uncheck();
    assert.equal(await page.evaluate(() => localStorage.getItem("tempo-v2-board-sounds")), "false");
    await page.reload();
    await page.getByRole("button", { name: "Reveal solution" }).waitFor();
    await move(page, "e2", "e4");
    await page.waitForFunction(() => [...document.querySelectorAll("cg-board piece")]
      .some(piece => piece.cgKey === "e5" && piece.cgPiece === "black pawn"));
    assert.deepEqual(await page.evaluate(() => [window.boardAudio.decoded, window.boardAudio.started]), [[], []]);

    // Load an opening lead-in only after enabling and priming audio in this tab.
    await page.getByRole("button", { name: "Settings" }).click();
    await page.getByRole("checkbox", { name: "Board sounds" }).check();
    card = { ...opening, attempt: 2, fen: afterReply.fen, legal: afterReply.legal,
      lead_in: { fen, moves: ["e2e4", "e7e5"] } };
    cardHeld = Promise.withResolvers();
    await page.reload();
    await prime();
    cardHeld.resolve();
    await heard(page, ["move-self", "move-opponent"]);
    await page.getByRole("heading", { name: "White to move" }).waitFor();
    await page.evaluate(() => {
      window.boardAudio.stopped.length = 0;
      const nextCard = new MutationObserver(() => {
        const button = document.querySelector('button[aria-keyshortcuts="Space"]:not(:disabled)');
        if (button) { nextCard.disconnect(); button.click(); }
      });
      nextCard.observe(document.querySelector("main"), { childList: true, subtree: true, attributes: true });
    });
    await move(page, "g1", "f3");
    await page.getByRole("heading", { name: "Play Nf3" }).waitFor();
    await page.waitForFunction(() => window.boardAudio.stopped.includes("move-self"));

    await page.getByRole("button", { name: "Openings", exact: true }).click();
    const saved = page.getByRole("button", { name: "Saved audio line white" });
    await saved.waitFor();
    await page.evaluate(() => { window.boardAudio.stopped.length = 0; });
    await move(page, "e2", "e4");
    await page.waitForFunction(() => window.boardAudio.started.at(-1) === "move-self");
    await saved.click();
    await page.waitForFunction(() => window.boardAudio.stopped.includes("move-self"));

    await page.getByRole("button", { name: "Tactics", exact: true }).click();
    await page.getByRole("button", { name: "Puzzle library", exact: true }).click();
    await page.getByRole("button", { name: "Audio replay" }).click();
    for (const destination of ["Audio replay", "Packs"]) {
      // Wait until the forward ply is committed, then navigate while its
      // recording is still playing.
      await page.evaluate(label => {
        window.boardAudio.stopped.length = 0;
        const afterPly = new MutationObserver(() => {
          if (!document.querySelector("main").textContent.includes("Position 1 of 3")) return;
          afterPly.disconnect();
          [...document.querySelectorAll("button")].find(button => button.textContent === label).click();
        });
        afterPly.observe(document.querySelector("main"), { childList: true, subtree: true, characterData: true });
        document.querySelector('[aria-label="Next move"]').click();
      }, destination);
      await page.waitForFunction(() => window.boardAudio.stopped.includes("move-self"));
    }
    assert.deepEqual(faults, []);
    console.log(`Chromium verified all seven recordings, silent retry, opponent replies, fast replay, opening playback, and saved mute. Drop to Web Audio start: Study ${studyDelay.toFixed(1)} ms, puzzle ${puzzleDelay.toFixed(1)} ms.`);
  } finally {
    await browser.close();
  }
}

qualify().catch(error => { console.error(error); process.exitCode = 1; });
