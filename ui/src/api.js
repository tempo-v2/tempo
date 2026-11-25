export async function api(path, data, signal) {
  const response = await fetch("/api/" + path, data === undefined ? { signal } : {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(data),
  });
  const body = await response.json().catch((error) => {
    if (response.ok && data === undefined) throw error;
    return null;
  });
  signal?.throwIfAborted();
  if (!response.ok) {
    const error = new Error(body?.error ?? `Request failed (${response.status}).`);
    error.code = body?.code;
    throw error;
  }
  return body;
}

export function fileText(event, setter, setError) {
  const file = event.target.files?.[0];
  if (!file) return;
  if (file.size > 64 * 1024 * 1024) { setError("Choose a PGN up to 64 MiB."); return; }
  file.text().then(setter, (error) => setError(error.message));
}

// Only disposable reads use this deadline. Durable commands keep their recovery rules.
export function readView(path, signal) {
  const deadline = AbortSignal.timeout(1000);
  return api(path, undefined, signal ? AbortSignal.any([signal, deadline]) : deadline);
}

export function visibleRead(load) {
  let request;
  let settled = false;
  const refresh = () => {
    if (document.hidden) {
      request?.abort();
    } else if (!request && !settled) {
      const current = new AbortController();
      request = current;
      void load(current.signal).finally(() => {
        request = undefined;
        settled = !current.signal.aborted;
        if (!settled && active) refresh();
      });
    }
  };
  let active = true;
  document.addEventListener("visibilitychange", refresh);
  refresh();
  return () => {
    active = false;
    request?.abort();
    document.removeEventListener("visibilitychange", refresh);
  };
}
