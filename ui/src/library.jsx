import React, { useEffect, useRef, useState } from "react";
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

function downloadOpeningDraft(draft) {
  const url = URL.createObjectURL(new Blob([JSON.stringify(draft)], { type: "application/json" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = "tempo-opening.json";
  link.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

const initial = { fen: new Chess().fen(), moves: [], start_at: 0 };

export function Openings({ fromGame, onSaved, sounds }) {
  const list = useList("openings");
  const [draft, setDraft] = useState(fromGame ?? initial);
  const [repertoires, setRepertoires] = useState([]);
  const [moreRepertoires, setMoreRepertoires] = useState(false);
  const [loadingRepertoires, setLoadingRepertoires] = useState(false);
  const [repertoireFailure, setRepertoireFailure] = useState(null);
  const repertoireRequest = useRef(null);
  const [destination, setDestination] = useState(fromGame ? "" : "new");
  const [branchChoice, setBranchChoice] = useState(null);
  const choiceKey = ({ id, color }) => id + "/" + color;
  const choices = branchChoice && !repertoires.some((entry) => choiceKey(entry) === choiceKey(branchChoice))
    ? [branchChoice, ...repertoires] : repertoires;
  const saveDestination = choices.find((entry) => choiceKey(entry) === destination);
  async function loadRepertoires(append = false) {
    if (repertoireRequest.current) {
      if (append) return;
      repertoireRequest.current.abort();
    }
    const request = new AbortController();
    repertoireRequest.current = request;
    setLoadingRepertoires(true);
    const tail = append ? repertoires.at(-1) : null;
    const cursor = tail ? "?" + new URLSearchParams({ after: tail.id, color: tail.color }) : "";
    try {
      const page = await api("openings/repertoires" + cursor, undefined, request.signal);
      if (request.signal.aborted) return;
      setRepertoires((rows) => append ? [...rows, ...page] : page);
      setMoreRepertoires(page.length === 50);
      setRepertoireFailure(null);
    } catch (error) {
      if (!request.signal.aborted) setRepertoireFailure({ message: error.message, append });
    } finally {
      if (repertoireRequest.current === request) {
        repertoireRequest.current = null;
        setLoadingRepertoires(false);
      }
    }
  }
  useEffect(() => {
    loadRepertoires();
    return () => {
      repertoireRequest.current?.abort();
      repertoireRequest.current = null;
    };
  }, []);
  const [name, setName] = useState("");
  const [color, setColor] = useState(fromGame?.color ?? "white");
  const [startingText, setStartingText] = useState("");
  const [opening, setOpening] = useState(null);
  const [ply, setPly] = useState(0);
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");
  const [offlineDraft, setOfflineDraft] = useState(null);
  const [busy, setBusy] = useState(false);
  const game = position(draft);
  const legal = game.moves({ verbose: true }).map(({ from, to, promotion }) => from + to + (promotion ?? ""));
  async function chooseStart(useExistingMoves = false) {
    setError("");
    try {
      let route;
      try { route = { fen: new Chess(startingText.trim()).fen(), moves: [] }; }
      catch { route = await api("openings/pgn", { pgn: startingText }); }
      setDraft({ ...draft, ...route, start_at: useExistingMoves ? 0 : route.moves.length });
      setOpening(null);
      setNotice("Position loaded. Play your continuation.");
    } catch (error) { setError(error.message); }
  }
  async function save(event) {
    event.preventDefault();
    if (!destination) return;
    setError("");
    setNotice("");
    setBusy(true);
    setOfflineDraft(null);
    const request = {
      name, color, route: { fen: draft.fen, moves: draft.moves }, start_at: draft.start_at,
      destination: destination === "new" ? { kind: "new" } : { kind: "existing", repertoire_id: saveDestination.id },
    };
    try {
      await api("openings", request);
      setNotice("Opening saved. Its responses are ready in Study.");
      onSaved();
      await list.load();
      if (destination === "new") loadRepertoires();
    } catch (error) {
      setError(error.message);
      if (error.code === "content_over_budget") setOfflineDraft(request);
    }
    finally { setBusy(false); }
  }
  return <section aria-label="Openings"><h1>Openings</h1>
    <div className="split">
      <div>
        {opening ? <><h2>{opening.name}</h2>
          <Replay sounds={sounds} route={opening.route} color={opening.color} ply={ply} onPly={setPly} />
          <button className="primary" onClick={() => {
            setDraft({
              ...opening.route, moves: opening.route.moves.slice(0, ply), start_at: ply,

            });
            setName(Array.from(opening.name).slice(0, 150).join("") + " variation");
            setColor(opening.color);
            const choice = { id: opening.repertoire_id, color: opening.color, example_line: opening.name };
            setBranchChoice(choice);
            setDestination(choiceKey(choice));
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
            <button onClick={() => { setDraft(initial); setDestination(fromGame ? "" : "new"); setNotice(""); }}>Start over</button>
            <span>{draft.moves.length - draft.start_at} continuation plies</span>
          </div>
        </>}
      </div>
      <div>
        {!opening && <form onSubmit={save}>
          <label>Repertoire<select required value={destination} onChange={(e) => {
            const next = e.target.value;
            setDestination(next);
            const entry = choices.find((item) => choiceKey(item) === next);
            if (entry) setColor(entry.color);
          }}>
            <option value="" disabled>Choose a repertoire</option>
            {choices.map((entry) => <option key={choiceKey(entry)} value={choiceKey(entry)}>{entry.example_line} ({entry.color})</option>)}
            <option value="new">New repertoire</option>
          </select></label>
          {repertoireFailure && <p className="error" role="alert">{repertoireFailure.message}</p>}
          {(moreRepertoires || repertoireFailure) && <button type="button" disabled={loadingRepertoires}
            onClick={() => loadRepertoires(repertoireFailure?.append ?? true)}>
            {repertoireFailure ? "Retry loading repertoires" : "Load more repertoires"}
          </button>}
          <label>Opening name<input required maxLength={160} value={name} onChange={(e) => setName(e.target.value)} /></label>
          <label>Practice color<select disabled={Boolean(saveDestination) || (destination !== "" && destination !== "new")} value={color} onChange={(e) => setColor(e.target.value)}>
            <option value="white">White</option><option value="black">Black</option>
          </select></label>
          <button className="primary" disabled={busy || !destination || draft.moves.length <= draft.start_at}>Save opening</button>
        </form>}
        {notice && <p role="status">{notice}</p>}
        {error && <p className="error" role="alert">{error}</p>}
        {offlineDraft && <button onClick={() => downloadOpeningDraft(offlineDraft)}>Download for offline import</button>}
        <details><summary>Start from PGN or FEN</summary>
          <textarea aria-label="Opening PGN or FEN" value={startingText} onChange={(e) => setStartingText(e.target.value)} />
          <label>Upload opening PGN<input type="file" accept=".pgn,text/plain" onChange={(e) => fileText(e, setStartingText, setError)} /></label>
          <button disabled={!startingText.trim()} onClick={() => chooseStart()}>Load position</button>
          <button disabled={!startingText.trim()} onClick={() => chooseStart(true)}>Use these moves as the line</button>
        </details>
        <h2>Saved openings</h2>
        {opening && <button onClick={() => setOpening(null)}>Back to editor</button>}
        <ul className="items">{list.rows.map((line) => <li key={line.id}>
          <button onClick={() => { setOpening(line); setPly(0); }}>{line.name} <small>{line.color}</small></button>
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
