const databaseName = "tempo-study-answer-journal";
const predecessorKey = "tempo.pending-answer.v2";

function openJournal() {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(databaseName, 1);
    request.onupgradeneeded = () => {
      request.result.createObjectStore("pending");
      request.result.createObjectStore("metadata");
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

async function journal(stores, mode, operation) {
  try {
    const db = await openJournal();
    try {
      return await new Promise((resolve, reject) => {
        const transaction = db.transaction(stores, mode);
        transaction.onabort = () => reject(transaction.error);
        // Resolve on commit. A request success alone does not mean it is durable.
        let request;
        transaction.oncomplete = () => resolve(request?.result);
        request = operation(transaction);
      });
    } finally {
      db.close();
    }
  } catch (cause) {
    throw new Error("Study could not access its saved Answers. Retry.", { cause });
  }
}

export function answerIntent({ dataset, attempt, played }) {
  if (typeof dataset !== "string" || !dataset || !Number.isSafeInteger(attempt) ||
      attempt <= 0 || !(played === null || (typeof played === "string" && /^[a-h][1-8][a-h][1-8][nbrq]?$/.test(played)))) {
    throw new Error("The saved Answer is invalid.");
  }
  return {
    version: 1,
    dataset,
    attempt,
    action: played === null ? { kind: "reveal" } : { kind: "play", move: played },
  };
}

export function answerPayload(intent) {
  if (intent?.version !== 1 || !["reveal", "play"].includes(intent.action?.kind)) {
    throw new Error("The saved Answer has an unsupported format.");
  }
  const played = intent.action.kind === "reveal" ? null : intent.action.move;
  answerIntent({ ...intent, played });
  return { dataset: intent.dataset, attempt: intent.attempt, played };
}

function identity(intent) {
  const { dataset, attempt, played } = answerPayload(intent);
  return [dataset, attempt, played ?? "reveal"];
}

export function saveAnswer(intent) {
  const key = identity(intent);
  return journal(["pending"], "readwrite", (tx) => tx.objectStore("pending").put(intent, key));
}

export function resolveAnswer(intent) {
  const key = identity(intent);
  return journal(["pending"], "readwrite", (tx) => tx.objectStore("pending").delete(key));
}

export function nextSavedAnswer(after) {
  return journal(["pending"], "readonly", (tx) =>
    tx.objectStore("pending").getAll(after ? IDBKeyRange.lowerBound(identity(after), true) : null, 1)
  ).then(([intent]) => intent);
}

export async function importPredecessorAnswer() {
  const raw = localStorage.getItem(predecessorKey);
  if (!raw) return;
  // Read only. An older tab could write a new scalar while this transaction runs.
  const intent = answerIntent(JSON.parse(raw));
  const key = identity(intent);
  await journal(["pending", "metadata"], "readwrite", (tx) => {
    const metadata = tx.objectStore("metadata");
    const previous = metadata.get(predecessorKey);
    previous.onsuccess = () => {
      if (previous.result === raw) return;
      tx.objectStore("pending").put(intent, key);
      metadata.put(raw, predecessorKey);
    };
  });
}
