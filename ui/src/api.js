export async function api(path, data) {
  const response = await fetch("/api/" + path, data === undefined ? {} : {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(data),
  });
  const body = await response.json().catch(() => null);
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
  if (file.size > 4 * 1024 * 1024) { setError("Choose a PGN up to 4 MiB."); return; }
  file.text().then(setter, (error) => setError(error.message));
}
