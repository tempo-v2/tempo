import React, { useEffect, useState } from "react";
import { api, fileText } from "./api.js";
import { Replay } from "./board.jsx";
import { More, useList } from "./library.jsx";

const labels = { chesscom: "Chess.com", lichess: "Lichess", pgn: "PGN", archive: "Archive scan" };

export function Settings() {
  const [names, setNames] = useState({ chesscom: "", lichess: "" });
  const [message, setMessage] = useState("");
  const [error, setError] = useState("");
  const [ready, setReady] = useState(false);
  useEffect(() => {
    api("settings").then((values) => { setNames((n) => ({ ...n, ...values })); setReady(true); })
      .catch((error) => setError(error.message));
  }, []);
  return <section><h1>Settings</h1>
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
    {error && <p className="error" role="alert">{error}</p>}
  </section>;
}

export function Games({ jump, addOpening, settings }) {
  const list = useList("games");
  const [game, setGame] = useState(null);
  const [ply, setPly] = useState(0);
  const [username, setUsername] = useState("");
  const [pgn, setPgn] = useState("");
  const [jobs, setJobs] = useState([]);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    let live = true;
    let timer;
    let prior = "";
    async function poll() {
      try {
        const jobs = await api("games/jobs");
        if (!live) return;
        const fingerprint = JSON.stringify(jobs.map(({ id, added, status }) => [id, added, status]));
        if (prior && prior !== fingerprint) list.load();
        prior = fingerprint;
        setJobs(jobs);
      } catch (error) { if (live) setError(error.message); }
      if (live) timer = setTimeout(poll, 2000);
    }
    poll();
    return () => { live = false; clearTimeout(timer); };
  }, []);
  useEffect(() => {
    if (!jump) return;
    let active = true;
    api("games/" + jump.id).then((result) => {
      if (active) { setGame(result); setPly(Math.min(jump.ply ?? 0, result.route.moves.length)); }
    }).catch((error) => { if (active) setError(error.message); });
    return () => { active = false; };
  }, [jump]);
  async function job(path, body) {
    setBusy(true); setError("");
    try { await api(path, body); setJobs(await api("games/jobs")); }
    catch (error) { setError(error.message); }
    finally { setBusy(false); }
  }
  return <section aria-label="Games"><h1>Games</h1>
    <div className="split">
      <div>{game && <>
        <h2>{game.white} vs {game.black}</h2>
        <Replay route={game.route} color={game.learner_color} ply={ply} onPly={setPly} />
        <div className="controls">
          <button className="primary" onClick={() => addOpening({
            fen: game.route.fen, moves: game.route.moves.slice(0, ply), start_at: ply, color: game.learner_color,
          })}>Add opening line</button>
          {game.source_url && <a href={game.source_url} target="_blank" rel="noreferrer">Original game</a>}
        </div>
      </>}</div>
      <div>
        <details open={!game}><summary>Import games</summary>
          {["chesscom", "lichess"].map((provider) => <div key={provider} className="provider">
            <strong>{labels[provider]}</strong>
            <button disabled={busy} onClick={() => job("games/jobs", { provider, operation: "all" })}>Import all games</button>
            <button disabled={busy} onClick={() => job("games/jobs", { provider, operation: "new" })}>Import new games</button>
          </div>)}
          <button onClick={settings}>Set provider usernames</button>
          <form onSubmit={(e) => { e.preventDefault(); job("games/jobs", { provider: "pgn", operation: "upload", username, pgn }); }}>
            <label>PGN player name<input required value={username} onChange={(e) => setUsername(e.target.value)} /></label>
            <label>Games PGN<textarea required value={pgn} onChange={(e) => setPgn(e.target.value)} /></label>
            <label>Upload games PGN<input type="file" accept=".pgn,text/plain" onChange={(e) => fileText(e, setPgn, setError)} /></label>
            <button className="primary" disabled={busy}>Import PGN</button>
          </form>
        </details>
        <div className="controls"><h2>Game archive</h2>
          <button disabled={busy} onClick={() => job("games/jobs", { provider: "archive", operation: "scan" })}>Scan archive</button>
        </div>
        {error && <p className="error" role="alert">{error}</p>}
        {jobs.length > 0 && <details open><summary>Recent jobs</summary><ul className="items">{jobs.map((j) =>
          <li key={j.id}><strong>{labels[j.provider]}</strong> · {j.status}
            <div>{j.added} added · {j.existing} already present · {j.rejected} rejected · {j.found} new puzzles</div>
            {j.error && <p className="error">{j.error}</p>}
            {j.status === "failed" && <button disabled={busy} onClick={() => job(`games/jobs/${j.id}/retry`, {})}>Retry import</button>}
          </li>)}</ul></details>}
        {!list.rows.length && <p>No games imported yet.</p>}
        <ul className="items">{list.rows.map((row) => <li key={row.id}><button onClick={() => { setGame(row); setPly(0); }}>
          {row.white} vs {row.black} <small>{row.played_on}</small>
        </button></li>)}</ul>
        <More list={list} />
      </div>
    </div>
  </section>;
}
