import React, { useEffect, useRef, useState } from "react";
import { Chess } from "chess.js";
import { api } from "./api.js";
import { ChessBoard, play } from "./board.jsx";

export function Study({ openGame }) {
  const [card, setCard] = useState(null);
  const [source, setSource] = useState(null);
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState("");
  const [pending, setPending] = useState(false);
  const waiting = useRef(false);
  const retry = useRef(null);
  const [epoch, setEpoch] = useState(0);
  async function run(op) {
    if (waiting.current) return;
    waiting.current = true;
    retry.current = op;
    setError("");
    setPending(true);
    try { await op(); retry.current = null; }
    catch (error) { setError(error.message); setEpoch((n) => n + 1); }
    finally { waiting.current = false; setPending(false); }
  }
  function next(after = null) {
    return run(async () => {
      setCard(await api("study/next", { after, dataset: card?.dataset }));
      setLoaded(true);
    });
  }
  useEffect(() => {
    run(async () => {
      const unresolved = localStorage.getItem("tempo.pending-answer.v2");
      if (unresolved) {
        try { await api("study/answer", JSON.parse(unresolved)); }
        catch (error) {
          if (!["stale_dataset", "answer_conflict"].includes(error.code)) throw error;
        }
        localStorage.removeItem("tempo.pending-answer.v2");
      }
      const active = await api("study");
      setCard(active ?? await api("study/next", { after: null }));
      setLoaded(true);
    });
  }, []);
  useEffect(() => {
    let active = true;
    setSource(null);
    if (card?.result) api(`study/${card.attempt}/context`).then((result) => {
      if (active) setSource(result);
    }).catch(() => {});
    return () => { active = false; };
  }, [card?.attempt, Boolean(card?.result)]);

  function answer(played) {
    const submission = { attempt: card.attempt, dataset: card.dataset, played };
    run(async () => {
      localStorage.setItem("tempo.pending-answer.v2", JSON.stringify(submission));
      const result = await api("study/answer", submission);
      localStorage.removeItem("tempo.pending-answer.v2");
      setCard((old) => ({ ...old, result }));
    });
  }

  return <section aria-label="Study">
    <h1>Study</h1>
    {error && <div className="error" role="alert">{error} <button onClick={() => run(retry.current)}>Retry</button></div>}
    {!loaded && !error && <p>Loading your deck…</p>}
    {loaded && !card && <><p>No cards are ready. Add an opening or tactic to get started.</p>
      <button onClick={() => next()} disabled={pending}>Check deck</button></>}
    {card && <>
      <h2>{card.result ? card.title : card.kind === "opening" ? "Play your opening response" : "Find the tactic"}</h2>
      <ChessBoard game={new Chess(card.fen)} legal={card.legal} orientation={card.color}
        identity={`study-${card.attempt}-${epoch}`} locked={pending || Boolean(error) || Boolean(card.result)}
        shapes={card.result?.answers.map((move) => ({ orig: move.slice(0, 2), dest: move.slice(2, 4), brush: "green" }))}
        onMove={card.result ? undefined : answer} />
      <div className="controls">
        {card.result ? <>
          <strong role="status">{card.result.rating}</strong>
          <button className="primary" disabled={pending} onClick={() => next(card.attempt)}>Next card</button>
        </> : <button disabled={pending || Boolean(error)} onClick={() => answer(null)}>Reveal answer</button>}
      </div>
      {card.result && <div className="feedback">
        <p>Answer: {card.result.answers.map((move) => play(new Chess(card.fen), move).san).join(" or ")}</p>
        {card.result.repeat_after !== null && <p>{card.result.repeat_after > 0 ? `Returns after ${card.result.repeat_after} cards.` : "Returns next."}</p>}
        {(card.result.played_in_game || source?.played) &&
          <p>In the game you played {play(new Chess(card.fen), card.result.played_in_game || source.played).san}.</p>}
        {source?.game_id && <button onClick={() => openGame(source.game_id, source.ply)}>View source game</button>}
      </div>}
    </>}
  </section>;
}
