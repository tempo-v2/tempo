import React, { useEffect, useState } from "react";
import { api, readView, visibleRead } from "./api.js";

const labels = { chesscom: "Chess.com", lichess: "Lichess" };

export function Settings({ sounds }) {
  const [names, setNames] = useState({ chesscom: "", lichess: "" });
  const [message, setMessage] = useState("");
  const [error, setError] = useState("");
  const [ready, setReady] = useState(false);
  const [readAttempt, retryRead] = useState(0);
  useEffect(() => visibleRead(async (signal) => {
    try {
      const values = await readView("settings", signal);
      if (signal.aborted) return;
      setNames((n) => ({ ...n, ...values }));
      setReady(true);
      setReadError("");
    } catch (failure) {
      if (!signal.aborted) setReadError(failure.message);
    }
  }), [readAttempt]);
  const [readError, setReadError] = useState("");
  return <section><h1>Settings</h1>
    <label className="board-sound-setting">
      <input type="checkbox" defaultChecked={sounds.enabled} onChange={(event) => sounds.setEnabled(event.target.checked)} />
      Board sounds
    </label>
    <NewOpeningSettings />
    <form className="settings" onSubmit={async (e) => {
      e.preventDefault();
      setMessage(""); setError("");
      try { await api("settings", names); setMessage("Saved."); }
      catch (error) { setError(error.message); }
    }}>
      {["chesscom", "lichess"].map((provider) => <label key={provider}>{labels[provider]} username
        <input value={names[provider]} onChange={(e) => setNames({ ...names, [provider]: e.target.value })} autoComplete="off" />
      </label>)}
      <button className="primary" disabled={!ready}>Save settings</button>
    </form>
    {message && <p role="status">{message}</p>}
    {readError && <p className="error" role="alert">{readError}{" "}
      <button onClick={() => retryRead((n) => n + 1)}>Retry usernames</button></p>}
    {error && <p className="error" role="alert">{error}</p>}
  </section>;
}

function NewOpeningSettings() {
  const [settings, setSettings] = useState(null);
  const [message, setMessage] = useState("");
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);

  const [readAttempt, retryRead] = useState(0);
  const [readError, setReadError] = useState("");
  useEffect(() => visibleRead(async (signal) => {
    try {
      const values = await readView("settings/new-openings", signal);
      if (signal.aborted) return;
      setSettings(values);
      setReadError("");
    } catch (failure) {
      if (!signal.aborted) setReadError(failure.message);
    }
  }), [readAttempt]);

  async function save(event) {
    event.preventDefault();
    setSaving(true);
    setMessage("");
    setError("");
    try {
      setSettings(await api("settings/new-openings", settings));
      setMessage("New opening card settings saved.");
    } catch (error) {
      setError(error.message);
    } finally {
      setSaving(false);
    }
  }

  return <section aria-labelledby="new-opening-settings">
    <h2 id="new-opening-settings">New opening cards</h2>
    {settings && <form className="settings" onSubmit={save}>
      <label className="board-sound-setting">
        <input type="checkbox" checked={settings.paused} disabled={saving}
          onChange={(event) => setSettings({ ...settings, paused: event.target.checked })} />
        Pause adding new openings to the queue
      </label>
      <label>New card order
        <select value={settings.ordering} disabled={saving}
          onChange={(event) => setSettings({ ...settings, ordering: event.target.value })}>
          <option value="maia">Maia · earlier moves, then most likely</option>
          <option value="standard">Standard</option>
        </select>
      </label>
      <small>These settings apply when Study adds cards to the queue. Scheduled reviews and already queued cards keep their place.</small>
      <button className="primary" disabled={saving}>Save opening settings</button>
    </form>}
    {message && <p role="status">{message}</p>}
    {readError && <p className="error" role="alert">{readError}{" "}
      <button onClick={() => retryRead((n) => n + 1)}>Retry opening preferences</button></p>}
    {error && <p className="error" role="alert">{error}</p>}
  </section>;
}
