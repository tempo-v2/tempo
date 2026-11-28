import { SimilarMoves } from "./similar_moves.jsx";
import { TacticPacks } from "./tactic_packs.jsx";
import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { Chess } from "chess.js";
import { api, fileText, readView } from "./api.js";
import { ChessBoard, Replay, play, position } from "./board.jsx";
import { moveCue } from "./sounds.js";
import { repertoireKey, useRepertoirePages } from "./repertoire_pages.js";

export function useList(endpoint) {
  const [rows, setRows] = useState([]);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);
  const [more, setMore] = useState(false);
  const controller = useRef(null);
  const load = useCallback((append = false) => controller.current?.(append), []);

  useEffect(() => {
    let active = true;
    let request;
    let refresh = true;
    let tail;
    async function read(append = false) {
      if (!active) return;
      if (!append) refresh = true;
      if (request || document.hidden || (append && !tail)) return;
      const current = new AbortController();
      request = current;
      const appendPage = append && !refresh;
      refresh = false;
      setBusy(true);
      try {
        const query = appendPage ? "?" + new URLSearchParams({ after: tail }) : "";
        const page = await readView(endpoint + query, current.signal);
        if (!active || current.signal.aborted || refresh) return;
        tail = page.at(-1)?.id;
        setRows((prior) => appendPage ? [...prior, ...page] : page);
        setMore(page.length === (endpoint === "games" ? 50 : 100));
        setError(null);
      } catch (failure) {
        if (active && !current.signal.aborted && !refresh) {
          setError({ message: failure.message, append: appendPage });
        }
      } finally {
        request = undefined;
        if (active) {
          setBusy(false);
          if (refresh && !document.hidden) void read();
        }
      }
    }
    function visibilityChanged() {
      if (document.hidden) {
        if (request) refresh = true;
        request?.abort();
      } else if (refresh) {
        void read();
      }
    }
    controller.current = read;
    document.addEventListener("visibilitychange", visibilityChanged);
    void read();
    return () => {
      active = false;
      controller.current = null;
      request?.abort();
      document.removeEventListener("visibilitychange", visibilityChanged);
    };
  }, [endpoint]);
  return { rows, busy, more, error, load };
}

export function More({ list }) {
  return <>{list.error && <p className="error" role="alert">{list.error.message}{" "}
    <button disabled={list.busy} onClick={() => list.load(list.error.append)}>Retry list</button></p>}
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

function decodeOpeningPreview(body) {
  const incomplete = "The opening preview reply was incomplete. Try again.";
  if (typeof body?.fen !== "string" || !body.fen || !Array.isArray(body.moves)) {
    throw new Error(incomplete);
  }
  const route = { fen: body.fen, moves: body.moves };
  try {
    const game = new Chess(route.fen);
    for (const move of route.moves) {
      if (play(game, move).lan !== move) throw new Error(incomplete);
    }
  } catch {
    throw new Error(incomplete);
  }
  return route;
}

export function Openings({ fromGame, onSaved, sounds }) {
  const list = useList("openings");
  const [draft, setDraft] = useState(fromGame ?? initial);
  const { rows: repertoires, more: moreRepertoires, busy: loadingRepertoires,
    error: repertoireFailure, load: loadRepertoires } = useRepertoirePages();
  const [destination, setDestination] = useState(fromGame ? null : "new");
  const choiceKey = repertoireKey;
  const saveDestination = destination === "new" ? null : destination;
  const choices = saveDestination && !repertoires.some((entry) => choiceKey(entry) === choiceKey(saveDestination))
    ? [saveDestination, ...repertoires] : repertoires;
  const [name, setName] = useState("");
  const [color, setColor] = useState(fromGame?.color ?? "white");
  const [startingText, setStartingText] = useState("");
  const [opening, setOpening] = useState(null);
  const [ply, setPly] = useState(0);
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");
  const [offlineDraft, setOfflineDraft] = useState(null);
  const [busy, setBusy] = useState(false);
  const sourceRequest = useRef(null);
  const [savedRevision, setSavedRevision] = useState(0);
  useLayoutEffect(() => sounds.cancel, [sounds, opening, color]);
  const game = position(draft);
  const legal = game.moves({ verbose: true }).map(({ from, to, promotion }) => from + to + (promotion ?? ""));

  function appendMove(move) {
    const board = position(draft);
    const played = play(board, move);
    sounds.play(moveCue(played, board));
    editDraft({ ...draft, moves: [...draft.moves, move] });
    setNotice("");
  }

  function cancelSource() {
    sourceRequest.current?.abort();
    sourceRequest.current = null;
  }
  useEffect(() => () => sourceRequest.current?.abort(), []);

  function beginSource() {
    cancelSource();
    const request = new AbortController();
    sourceRequest.current = request;
    return request;
  }

  function finishSource(request) {
    if (sourceRequest.current === request) sourceRequest.current = null;
  }

  function editDraft(next) {
    cancelSource();
    setDraft(next);
  }
  function resetDraft(next) {
    sounds.cancel();
    editDraft(next);
  }
  function changeStartingText(text) {
    cancelSource();
    setStartingText(text);
  }
  async function readStartingFile(event) {
    if (!event.target.files?.length) return;
    const request = beginSource();
    try {
      await fileText(event, setStartingText, setError, request.signal);
    } finally {
      finishSource(request);
    }
  }

  async function chooseStart(useExistingMoves = false) {
    const request = beginSource();
    setError("");
    try {
      let route;
      try { route = { fen: new Chess(startingText.trim()).fen(), moves: [] }; }
      catch {
        const body = await api("openings/pgn", { pgn: startingText }, request.signal, { requireJson: true });
        if (request.signal.aborted) return;
        route = decodeOpeningPreview(body);
      }
      sounds.cancel();
      setDraft({ ...draft, ...route, start_at: useExistingMoves ? 0 : route.moves.length });
      setOpening(null);
      setNotice("Position loaded. Play your continuation.");
    } catch (error) {
      if (!request.signal.aborted) setError(error.message);
    } finally {
      finishSource(request);
    }
  }
  async function save(event) {
    event.preventDefault();
    if (!destination) return;
    cancelSource();
    setError("");
    setNotice("");
    setBusy(true);
    setOfflineDraft(null);
    const request = {
      name, color: saveDestination?.color ?? color, route: { fen: draft.fen, moves: draft.moves }, start_at: draft.start_at,
      destination: destination === "new" ? { kind: "new" } : { kind: "existing", repertoire_id: saveDestination.id },
    };
    try {
      await api("openings", request);
      setNotice("Opening saved. New responses enter the next daily queue.");
      setSavedRevision(revision => revision + 1);
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
            editDraft({
              ...opening.route, moves: opening.route.moves.slice(0, ply), start_at: ply,

            });
            setName(Array.from(opening.name).slice(0, 150).join("") + " variation");
            setColor(opening.color);
            setDestination({
              id: opening.repertoire_id, color: opening.color,
              example_line: opening.repertoire_example ?? opening.name,
            });
            setOpening(null);
          }}>Branch here</button>
        </> : <>
          <ChessBoard game={game} legal={legal} orientation={color} identity="opening-editor"
            onRejectedMove={() => sounds.play("illegal")}
            onMove={appendMove} />
          <div className="controls">
            <button disabled={draft.moves.length <= draft.start_at} onClick={() => resetDraft({ ...draft, moves: draft.moves.slice(0, -1) })}>Undo</button>
            <button onClick={() => { resetDraft(initial); setDestination(fromGame ? null : "new"); setNotice(""); }}>Start over</button>
            <span>{draft.moves.length - draft.start_at} continuation plies</span>
          </div>
        </>}
      </div>
      <div>
        {!opening && <form onSubmit={save}>
          <label>Repertoire<select required value={saveDestination ? choiceKey(saveDestination) : destination ?? ""} onChange={(e) => {
            const next = e.target.value;
            const entry = choices.find((item) => choiceKey(item) === next);
            setDestination(next === "new" ? "new" : entry ?? null);
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
          <label>Practice color<select disabled={Boolean(saveDestination)} value={color} onChange={(e) => setColor(e.target.value)}>
            <option value="white">White</option><option value="black">Black</option>
          </select></label>
          <button className="primary" disabled={busy || !destination || draft.moves.length <= draft.start_at}>Save opening</button>
        </form>}
        {!opening && saveDestination && !busy && <SimilarMoves
          key={JSON.stringify([draft, saveDestination.id, saveDestination.color, savedRevision])}
          game={game} repertoire={saveDestination} onUse={appendMove} />}
        {notice && <p role="status">{notice}</p>}
        {error && <p className="error" role="alert">{error}</p>}
        {offlineDraft && <button onClick={() => downloadOpeningDraft(offlineDraft)}>Download for offline import</button>}
        <details><summary>Start from PGN or FEN</summary>
          <textarea aria-label="Opening PGN or FEN" value={startingText} onChange={(e) => changeStartingText(e.target.value)} />
          <label>Upload opening PGN<input type="file" accept=".pgn,text/plain" onChange={readStartingFile} /></label>
          <button disabled={!startingText.trim()} onClick={() => chooseStart()}>Load position</button>
          <button disabled={!startingText.trim()} onClick={() => chooseStart(true)}>Use these moves as the line</button>
        </details>
        <h2>Saved openings</h2>
        {opening && <button onClick={() => { cancelSource(); setOpening(null); }}>Back to editor</button>}
        <ul className="items">{list.rows.map((line) => <li key={line.id}>
          <button onClick={() => { cancelSource(); setOpening(line); setPly(0); }}>{line.name} <small>{line.color}</small></button>
        </li>)}</ul>
        <More list={list} />
      </div>
    </div>
  </section>;
}

export function Tactics({ openGame, sounds }) {
  const [page, setPage] = useState("packs");
  return <section aria-label="Tactics">
    <h1>Tactics</h1>
    <div className="controls">
      <button aria-pressed={page === "packs"} onClick={() => setPage("packs")}>Packs</button>
      <button aria-pressed={page === "library"} onClick={() => setPage("library")}>Puzzle library</button>
    </div>
    {page === "packs" ? <TacticPacks sounds={sounds} /> : <TacticLibrary openGame={openGame} sounds={sounds} />}
  </section>;
}

function TacticLibrary({ openGame, sounds }) {
  const list = useList("tactics");
  const [name, setName] = useState("");
  const [pgn, setPgn] = useState("");
  const [puzzleUrl, setPuzzleUrl] = useState("");
  const [formatChoice, setFormatChoice] = useState(null);
  const [setupChoice, setSetupChoice] = useState(null);
  const [detection, setDetection] = useState(null);
  const [detectionRetry, setDetectionRetry] = useState(0);
  const [problem, setProblem] = useState(null);
  const [ply, setPly] = useState(0);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [readingFile, setReadingFile] = useState(false);
  const fileRead = useRef(null);

  const currentDetection = detection?.pgn === pgn && detection.puzzleUrl === puzzleUrl
    && detection.retry === detectionRetry ? detection : null;
  const format = formatChoice ?? currentDetection?.options?.format ?? "generic_pgn";
  const setup = setupChoice ?? (format === "chess_com_puzzle");
  const hasSetupChoice = formatChoice !== null || setupChoice !== null;
  const ready = currentDetection?.options || (currentDetection?.error && hasSetupChoice);
  const canSave = Boolean(pgn.trim() && ready && !readingFile && !busy);
  const checking = Boolean((pgn.trim() || puzzleUrl.trim()) && !currentDetection);

  useEffect(() => {
    if (!pgn.trim() && !puzzleUrl.trim()) return;
    const controller = new AbortController();
    const { signal } = controller;
    const timer = setTimeout(async () => {
      const request = { pgn, puzzleUrl, retry: detectionRetry };
      try {
        const options = await api("tactics/import-options", { pgn, puzzle_url: puzzleUrl }, signal, { requireJson: true });
        setDetection({ ...request, options });
      } catch (failure) {
        if (!signal.aborted) setDetection({ ...request, error: failure.message });
      }
    }, 250);
    return () => {
      clearTimeout(timer);
      controller.abort();
    };
  }, [pgn, puzzleUrl, detectionRetry]);

  useEffect(() => () => fileRead.current?.abort(), []);

  function changePgn(text) {
    fileRead.current?.abort();
    setReadingFile(false);
    setError("");
    setPgn(text);
  }

  async function readPuzzleFile(event) {
    if (!event.target.files?.length) return;
    fileRead.current?.abort();
    const controller = new AbortController();
    fileRead.current = controller;
    setReadingFile(true);
    setError("");
    const read = fileText(event, setPgn, setError, controller.signal);
    event.target.value = "";
    try { await read; }
    finally {
      if (!controller.signal.aborted) {
        fileRead.current = null;
        setReadingFile(false);
      }
    }
  }

  function openPuzzle(row) {
    setProblem(row);
    setPly(0);
  }

  async function save(event) {
    event.preventDefault();
    if (!canSave) return;
    setError("");
    setBusy(true);
    try {
      const saved = await api("tactics", {
        name, pgn, puzzle_url: puzzleUrl, format: formatChoice, first_move_is_setup: setupChoice,
      }, undefined, { requireJson: true });
      openPuzzle(saved.puzzle);
      setPgn("");
      setName("");
      setPuzzleUrl("");
      setFormatChoice(null);
      setSetupChoice(null);
      setDetection(null);
      await list.load();
    } catch (failure) {
      setError(failure.message);
    } finally {
      setBusy(false);
    }
  }

  return <section aria-label="Puzzle library">
    <div className="split">
      <div>{problem && <>
        <h2>{problem.name}</h2>
        <Replay sounds={sounds} route={problem.route} color={position(problem.route, 0).turn() === "w" ? "white" : "black"} ply={ply} onPly={setPly} />
        {problem.source_url && <a href={problem.source_url} target="_blank" rel="noreferrer">Open on Chess.com</a>}
        {problem.source_game && <button onClick={() => openGame(problem.source_game, problem.source_ply)}>View source game</button>}
      </>}</div>
      <div><form onSubmit={save}>
        <fieldset className="puzzle-import-fields" disabled={busy}>
          <label>Puzzle name<input value={name} required maxLength={160} onChange={(e) => setName(e.target.value)} /></label>
          <label>Puzzle PGN<textarea required value={pgn} onChange={(e) => changePgn(e.target.value)} /></label>
          <label>Upload puzzle PGN<input type="file" accept=".pgn,text/plain" onChange={readPuzzleFile} /></label>
          <label>Puzzle URL (optional)<input value={puzzleUrl} inputMode="url" maxLength={2048} onChange={(e) => { setError(""); setPuzzleUrl(e.target.value); }} /></label>
          <label>Import format<select value={format} onChange={(e) => { setError(""); setFormatChoice(e.target.value); }}>
            <option value="generic_pgn">Generic PGN</option>
            <option value="chess_com_puzzle">Chess.com puzzle</option>
          </select></label>
          <label className="checkbox-label"><input type="checkbox" checked={setup} onChange={(e) => { setError(""); setSetupChoice(e.target.checked); }} /> First move is setup</label>
        </fieldset>
        {checking && <p role="status">Checking import format…</p>}
        {currentDetection?.error && <p className="error" role="alert">{currentDetection.error}{" "}
          <button type="button" onClick={() => setDetectionRetry((retry) => retry + 1)}>Retry format check</button>
        </p>}
        <button className="primary" disabled={!canSave}>Add puzzle</button>
      </form>
      {error && <p className="error" role="alert">{error}</p>}
      <h2>Puzzle library</h2>
      {!list.rows.length && <p>No puzzles saved yet.</p>}
      <ul className="items">{list.rows.map((row) => <li key={row.id}><button onClick={() => openPuzzle(row)}>{row.name}</button></li>)}</ul>
      <More list={list} /></div>
    </div>
  </section>;
}
