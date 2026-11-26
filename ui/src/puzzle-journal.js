import { isMove } from "./answer-journal.js";

function openJournal() {
  return new Promise((resolve, reject) => {
    // Leave the ordinary Answer database at version 1 for still-loaded tabs.
    const request = indexedDB.open("tempo-puzzle-command-journal", 1);
    request.onupgradeneeded = () => request.result.createObjectStore("pending");
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

async function pending(mode, operation) {
  try {
    const db = await openJournal();
    try {
      return await new Promise((resolve, reject) => {
        const transaction = db.transaction("pending", mode);
        transaction.onabort = () => reject(transaction.error);
        let request;
        transaction.oncomplete = () => resolve(request?.result);
        request = operation(transaction.objectStore("pending"), transaction);
      });
    } finally {
      db.close();
    }
  } catch (cause) {
    throw new Error("Tempo could not access its saved puzzle commands. Retry.", { cause });
  }
}

function commandKey(scope, command) {
  const intent = command?.intent;
  if (typeof intent?.dataset === "string" && intent.dataset) {
    if (["study", "tactics"].includes(scope) && command.kind === "move" &&
        Number.isSafeInteger(intent.play) && intent.play > 0 &&
        Number.isSafeInteger(intent.step) && intent.step >= 0 &&
        (intent.played === null || isMove(intent.played))) {
      return [scope, intent.dataset, "move", intent.play, intent.step, intent.played ?? "reveal"];
    }
    if (scope === "tactics" && command.kind === "start" &&
        typeof intent.request === "string" && /^[a-f0-9-]{36}$/i.test(intent.request) &&
        typeof intent.puzzle === "string" && intent.puzzle &&
        typeof intent.revision === "string" && intent.revision && typeof intent.practice === "boolean") {
      return [scope, intent.dataset, "start", intent.request];
    }
  }
  throw new Error("The saved puzzle command is invalid.");
}

export function savePuzzleCommand(scope, command) {
  const key = commandKey(scope, command);
  return pending("readwrite", (store, transaction) => {
    const prior = store.get(key);
    prior.onsuccess = () => {
      if (!prior.result) {
        store.add(command, key);
      } else if (command.kind === "start") {
        const { puzzle, revision, practice } = prior.result.intent;
        const intent = command.intent;
        if (puzzle !== intent.puzzle || revision !== intent.revision || practice !== intent.practice) {
          transaction.abort();
        }
      }
    };
  });
}

export function resolvePuzzleCommand(scope, command) {
  const key = commandKey(scope, command);
  return pending("readwrite", (store) => store.delete(key));
}

export async function nextPuzzleCommand(scope, after) {
  // The array prefix limits recovery to the mounting controller's surface.
  const range = IDBKeyRange.bound(after ?? [scope], [scope, []], true, true);
  const [command] = await pending("readonly", (store) => store.getAll(range, 1));
  return command ? { command, key: commandKey(scope, command) } : null;
}

export async function importPuzzleCommand(scope) {
  const key = `tempo.lichess.v1.${scope}`;
  const previous = sessionStorage.getItem(key);
  if (!previous) return;
  await savePuzzleCommand(scope, JSON.parse(previous));
  if (sessionStorage.getItem(key) === previous) sessionStorage.removeItem(key);
}
