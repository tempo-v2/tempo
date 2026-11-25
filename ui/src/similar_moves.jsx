import React, { useEffect, useRef, useState } from "react";
import { api } from "./api.js";
import { ChessBoard } from "./board.jsx";

function defaultMove(similarity, engine) {
  if (similarity?.status !== "ranked" || engine?.status !== "ranked") return null;
  const coverage = similarity.coverage;
  const approved = new Set(engine.first_moves);
  if (coverage.kind === "conflict") return null;
  return similarity.moves.find(({ first_move }) =>
    approved.has(first_move) && (coverage.kind === "gap" || coverage.first_move === first_move)
  )?.first_move ?? null;
}

export function SimilarMoves({ game, repertoire, onUse }) {
  const [similarity, setSimilarity] = useState(null);
  const [engine, setEngine] = useState(null);
  const [chosen, setChosen] = useState(null);
  const [searching, setSearching] = useState(false);
  const [error, setError] = useState(null);
  const current = useRef(null);
  const fen = game.fen();

  useEffect(() => {
    function hide() {
      if (!document.hidden || !current.current) return;
      current.current.abort();
      current.current = null;
      setSearching(false);
    }
    document.addEventListener("visibilitychange", hide);
    return () => {
      current.current?.abort();
      document.removeEventListener("visibilitychange", hide);
    };
  }, []);

  async function search() {
    current.current?.abort();
    const request = new AbortController();
    current.current = request;
    setSearching(true);
    setSimilarity(null);
    setEngine(null);
    setChosen(null);
    setError(null);
    const { signal } = request;
    const similar = api("openings/similar-moves", {
      fen, repertoire_id: repertoire.id, color: repertoire.color,
    }, AbortSignal.any([signal, AbortSignal.timeout(1000)]), { requireJson: true })
      .then(setSimilarity)
      .catch(failure => { if (!signal.aborted) setError(failure.message); });
    const evaluated = api("openings/engine-moves", { fen },
      AbortSignal.any([signal, AbortSignal.timeout(5500)]), { requireJson: true })
      .then(setEngine)
      .catch(() => { if (!signal.aborted) setEngine({ status: "unavailable" }); });
    await Promise.all([similar, evaluated]);
    if (current.current === request) {
      current.current = null;
      setSearching(false);
    }
  }

  const moves = similarity?.status === "ranked" ? similarity.moves : [];
  const selected = chosen ?? defaultMove(similarity, engine);
  const arrow = selected ? [{ orig: selected.slice(0, 2), dest: selected.slice(2, 4), brush: "green" }] : [];

  return <section aria-label="Similar moves">
    <button type="button" disabled={searching} onClick={search}>Find similar moves</button>
    {searching && <span role="status"> Searching…</span>}
    {error && <p role="alert">{error}</p>}
    {similarity?.status === "terminal" && <p>This position has no legal moves.</p>}
    {similarity?.status === "opponent_to_move" && <p>Similar moves are available for your repertoire side.</p>}
    {similarity?.coverage.kind === "conflict" && <p>This position has an unresolved repertoire conflict.</p>}
    {similarity?.status === "ranked" && !moves.length && <p>No legal saved replies match this position.</p>}
    {moves.length > 0 && <>
      <div className="similar-moves-board">
        <ChessBoard game={game} orientation={repertoire.color} identity="similar-moves" shapes={arrow} />
      </div>
      <div className="controls" role="group" aria-label="Familiar moves">
        {moves.map(({ first_move, san }) =>
          <button type="button" key={first_move} aria-pressed={first_move === selected}
            onClick={() => setChosen(first_move)}>
            {san}{similarity.coverage.first_move === first_move ? " (saved)" : ""}
          </button>
        )}
      </div>
      {["busy", "unavailable"].includes(engine?.status) && <p>Stockfish is temporarily unavailable.</p>}
      <button type="button" disabled={!selected} onClick={() => onUse(selected)}>Use move</button>
    </>}
  </section>;
}
