import React, { useEffect, useState } from "react";
import { Chess } from "chess.js";
import { api, fileText } from "./api.js";
import { ChessBoard, Replay, play, position } from "./board.jsx";
import { moveCue } from "./sounds.js";

export function useList(endpoint) {
  const [rows, setRows] = useState([]);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [more, setMore] = useState(false);
  async function load(append = false) {
    setBusy(true);
    try {
      const page = await api(endpoint + (append ? "?after=" + rows.at(-1).id : ""));
      setRows(append ? [...rows, ...page] : page);
      setMore(page.length === (endpoint === "games" ? 50 : 100));
      setError("");
    } catch (error) { setError(error.message); }
    finally { setBusy(false); }
  }
  useEffect(() => { load(); }, [endpoint]);
  return { rows, busy, more, error, load };
}

export function More({ list }) {
  return <>{list.error && <p className="error" role="alert">{list.error}</p>}
    {list.more && <button disabled={list.busy} onClick={() => list.load(true)}>Load more</button>}</>;
}

const initial = { fen: new Chess().fen(), moves: [], start_at: 0 };

export function Openings({ fromGame, onSaved, sounds }) {
  const list = useList("openings");
  const [draft, setDraft] = useState(fromGame ?? initial);
  const [name, setName] = useState("");
  const [color, setColor] = useState(fromGame?.color ?? "white");
  const [startingText, setStartingText] = useState("");
  const [opening, setOpening] = useState(null);
  const [ply, setPly] = useState(0);
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const game = position(draft);
  const legal = game.moves({ verbose: true }).map(({ from, to, promotion }) => from + to + (promotion ?? ""));
  async function chooseStart(useExistingMoves = false) {
    setError("");
    try {
      let route;
      try { route = { fen: new Chess(startingText.trim()).fen(), moves: [] }; }
      catch { route = await api("openings/pgn", { pgn: startingText }); }
      setDraft({ ...route, start_at: useExistingMoves ? 0 : route.moves.length });
      setOpening(null);
      setNotice("Position loaded. Play your continuation.");
    } catch (error) { setError(error.message); }
  }
  async function save(event) {
    event.preventDefault();
    setError("");
    setNotice("");
    setBusy(true);
    try {
      await api("openings", { name, color, route: { fen: draft.fen, moves: draft.moves }, start_at: draft.start_at });
      setNotice("Opening saved. Its responses are ready in Study.");
      await list.load();
      onSaved();
    } catch (error) { setError(error.message); }
    finally { setBusy(false); }
  }
  return <section aria-label="Openings"><h1>Openings</h1>
    <div className="split">
      <div>
        {opening ? <><h2>{opening.name}</h2>
          <Replay sounds={sounds} route={opening.route} color={opening.color} ply={ply} onPly={setPly} />
          <button className="primary" onClick={() => {
            setDraft({ ...opening.route, moves: opening.route.moves.slice(0, ply), start_at: ply });
            setName(opening.name + " variation");
            setColor(opening.color);
            setOpening(null);
          }}>Branch here</button>
        </> : <>
          <ChessBoard game={game} legal={legal} orientation={color} identity="opening-editor"
            onRejectedMove={() => sounds.play("illegal")}
            onMove={(move) => {
              const board = position(draft);
              const played = play(board, move);
              sounds.play(moveCue(played, board));
              setDraft({ ...draft, moves: [...draft.moves, move] });
              setNotice("");
            }} />
          <div className="controls">
            <button disabled={draft.moves.length <= draft.start_at} onClick={() => setDraft({ ...draft, moves: draft.moves.slice(0, -1) })}>Undo</button>
            <button onClick={() => { setDraft(initial); setNotice(""); }}>Start over</button>
            <span>{draft.moves.length - draft.start_at} continuation plies</span>
          </div>
        </>}
      </div>
      <div>
        {!opening && <form onSubmit={save}>
          <label>Opening name<input required maxLength={160} value={name} onChange={(e) => setName(e.target.value)} /></label>
          <label>Practice color<select value={color} onChange={(e) => setColor(e.target.value)}>
            <option value="white">White</option><option value="black">Black</option>
          </select></label>
          <button className="primary" disabled={busy || draft.moves.length <= draft.start_at}>Save opening</button>
        </form>}
        {notice && <p role="status">{notice}</p>}
        {error && <p className="error" role="alert">{error}</p>}
        <details><summary>Start from PGN or FEN</summary>
          <textarea aria-label="Opening PGN or FEN" value={startingText} onChange={(e) => setStartingText(e.target.value)} />
          <label>Upload opening PGN<input type="file" accept=".pgn,text/plain" onChange={(e) => fileText(e, setStartingText, setError)} /></label>
          <button disabled={!startingText.trim()} onClick={() => chooseStart()}>Load position</button>
          <button disabled={!startingText.trim()} onClick={() => chooseStart(true)}>Use these moves as the line</button>
        </details>
        <h2>Saved openings</h2>
        {opening && <button onClick={() => setOpening(null)}>Back to editor</button>}
        <ul className="items">{list.rows.map((line) => <li key={line.id}>
          <button onClick={() => { setOpening(line); setPly(line.start_at); }}>{line.name} <small>{line.color}</small></button>
        </li>)}</ul>
        <More list={list} />
      </div>
    </div>
  </section>;
}

export function Tactics({ openGame, sounds }) {
  const list = useList("tactics");
  const [name, setName] = useState("");
  const [pgn, setPgn] = useState("");
  const [problem, setProblem] = useState(null);
  const [ply, setPly] = useState(0);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  async function save(event) {
    event.preventDefault();
    setError("");
    setBusy(true);
    try { await api("tactics", { name, pgn }); setPgn(""); setName(""); await list.load(); }
    catch (error) { setError(error.message); }
    finally { setBusy(false); }
  }
  return <section aria-label="Tactics"><h1>Tactics</h1>
    <div className="split">
      <div>{problem && <>
        <h2>{problem.name}</h2>
        <Replay sounds={sounds} route={problem.route} color={position(problem.route, 0).turn() === "w" ? "white" : "black"} ply={ply} onPly={setPly} />
        {problem.source_game && <button onClick={() => openGame(problem.source_game, problem.source_ply)}>View source game</button>}
      </>}</div>
      <div><form onSubmit={save}>
        <label>Puzzle name<input value={name} required maxLength={160} onChange={(e) => setName(e.target.value)} /></label>
        <label>Puzzle PGN<textarea required placeholder="PGN starting at the solver’s turn" value={pgn} onChange={(e) => setPgn(e.target.value)} /></label>
        <label>Upload puzzle PGN<input type="file" accept=".pgn,text/plain" onChange={(e) => fileText(e, setPgn, setError)} /></label>
        <button className="primary" disabled={busy}>Add puzzle</button>
      </form>
      {error && <p className="error" role="alert">{error}</p>}
      <h2>Puzzle library</h2>
      {!list.rows.length && <p>No puzzles saved yet.</p>}
      <ul className="items">{list.rows.map((row) => <li key={row.id}><button onClick={() => { setProblem(row); setPly(0); }}>{row.name}</button></li>)}</ul>
      <More list={list} /></div>
    </div>
  </section>;
}
