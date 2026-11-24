import React, { useEffect, useMemo, useRef, useState } from "react";
import { Chess } from "chess.js";
import { api } from "./api.js";
import { ChessBoard, play } from "./board.jsx";
import { moveCue } from "./sounds.js";

const successPauseMs = 800;
const openingPlyMs = 260;

function openingFrames(route, targetFen) {
  if (!route?.moves?.length) return null;
  try {
    const game = new Chess(route.fen);
    const frames = [{ fen: game.fen() }];
    for (const uci of route.moves) {
      const { from, to, color } = play(game, uci);
      frames.push({ fen: game.fen(), lastMove: [from, to], movedBy: color });
    }
    // Display context may be missing in older content or invalid. The issued
    // position remains the sole input and grading position.
    return game.fen() === new Chess(targetFen).fen() ? frames : null;
  } catch {
    return null;
  }
}

function markedMove(move, brush) {
  if (!move) return [];
  const [orig, dest] = move;
  return [{ orig, dest, brush }, { orig: dest, brush }];
}

function validStudyCount(value) {
  return (Number.isSafeInteger(value) && value >= 0) ||
    (Number.isSafeInteger(value?.at_least) && value.at_least > 0);
}

function formatStudyCount(value) {
  return typeof value === "number" ? String(value) : `${value.at_least.toLocaleString("en-US")}+`;
}

function useStudyCounts() {
  const [counts, setCounts] = useState(null);
  const invalidate = useRef(null);
  useEffect(() => {
    let active = true;
    let pending = null;
    let dirty = false;
    async function refresh() {
      if (!active || pending) return;
      const request = new AbortController();
      pending = request;
      dirty = false;
      const timeout = setTimeout(() => request.abort(), 1000);
      try {
        const response = await fetch("/api/study/counts", { signal: request.signal });
        const totals = response.ok ? await response.json() : null;
        const valid = [totals?.new, totals?.reviews].every(validStudyCount);
        if (active && !dirty) setCounts(valid ? totals : null);
      } catch {
        if (active && !dirty) setCounts(null);
      } finally {
        clearTimeout(timeout);
        pending = null;
        if (dirty) refresh();
      }
    }
    invalidate.current = () => {
      setCounts(null);
      dirty = true;
      refresh();
    };
    window.addEventListener("focus", refresh);
    return () => {
      active = false;
      invalidate.current = null;
      pending?.abort();
      window.removeEventListener("focus", refresh);
    };
  }, []);
  return [counts, invalidate];
}

export function Study({ openGame, sounds }) {
  const [card, setCard] = useState(null);
  const [counts, invalidateCounts] = useStudyCounts();
  const [sourceRead, setSource] = useState(null);
  const [opponent, setOpponent] = useState(null);
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState(null);
  const [pending, setPending] = useState(false);
  const [submitted, setSubmitted] = useState(null);
  const waiting = useRef(false);
  const retry = useRef(null);
  const nextButton = useRef(null);
  const opponentRead = useRef(null);
  const [promotionTarget, setPromotionTarget] = useState(null);
  const [epoch, setEpoch] = useState(0);
  const [advanceAfter, setAdvanceAfter] = useState(null);
  const mounted = useRef(true);
  const [openingProgress, setOpeningProgress] = useState(null);
  const attemptKey = card ? `${card.dataset}/${card.attempt}` : null;
  const source = sourceRead?.key === attemptKey ? sourceRead.result : null;
  const frames = useMemo(() => openingFrames(card?.lead_in, card?.fen), [card?.lead_in, card?.fen]);
  const openingPly = openingProgress?.key === attemptKey ? openingProgress.ply : 0;
  const lastPly = frames ? frames.length - 1 : 0;
  const openingPlaying = Boolean(frames && !card?.result && openingPly < lastPly);

  useEffect(() => {
    if (!openingPlaying) return;
    const timer = setTimeout(() => {
      setOpeningProgress((current) => current?.key === attemptKey && current.ply > openingPly
        ? current : { key: attemptKey, ply: openingPly + 1 });
    }, openingPlyMs);
    return () => clearTimeout(timer);
  }, [attemptKey, openingPlaying, openingPly]);
  async function run(op) {
    if (waiting.current) return;
    waiting.current = true;
    retry.current = op;
    setError(null);
    setPending(true);
    try { await op(); retry.current = null; }
    catch (error) { setError(error); setEpoch((n) => n + 1); }
    finally { waiting.current = false; setPending(false); }
  }
  function next(after = null) {
    setAdvanceAfter(null);
    return run(async () => {
      setCard(await api("study/next", { after, dataset: card?.dataset }));
      setLoaded(true);
      invalidateCounts.current?.();
    });
  }
  useEffect(() => {
    mounted.current = true;
    run(async () => {
      const unresolved = localStorage.getItem("tempo.pending-answer.v2");
      let replayAccepted = false;
      if (unresolved) {
        try {
          await api("study/answer", JSON.parse(unresolved));
          replayAccepted = true;
          invalidateCounts.current?.();
        }
        catch (error) {
          if (!["stale_dataset", "answer_conflict"].includes(error.code)) throw error;
        }
        localStorage.removeItem("tempo.pending-answer.v2");
      }
      const active = await api("study");
      setCard(active ?? await api("study/next", { after: null }));
      setLoaded(true);
      if (!replayAccepted || !active) invalidateCounts.current?.();
    });
    return () => { mounted.current = false; sounds.cancel(); };
  }, []);
  useEffect(() => {
    if (!attemptKey || card.kind === "opening") return;
    const request = new AbortController();
    opponentRead.current = api(`study/${card.attempt}/opponent-move`, undefined, request.signal)
      .then((move) => {
        if (!request.signal.aborted && move) {
          setOpponent({ key: attemptKey, move: [move.slice(0, 2), move.slice(2, 4)] });
        }
      }).catch(() => {});
    return () => {
      request.abort();
      opponentRead.current = null;
    };
  }, [attemptKey, card?.kind]);

  useEffect(() => {
    setSource(null);
    if (!card?.result) return;
    const request = new AbortController();
    Promise.resolve(opponentRead.current).then(async () => {
      if (request.signal.aborted) return;
      const result = await api(`study/${card.attempt}/context`, undefined, request.signal);
      if (!request.signal.aborted) setSource({ key: attemptKey, result });
    }).catch(() => {});
    return () => request.abort();
  }, [attemptKey, Boolean(card?.result)]);

  useEffect(() => {
    if (advanceAfter === null) return;
    const timer = setTimeout(() => next(advanceAfter), successPauseMs);
    return () => clearTimeout(timer);
  }, [advanceAfter]);

  useEffect(() => {
    function advanceOnSpace(event) {
      const button = nextButton.current;
      if (!button || button.disabled || event.defaultPrevented || event.isComposing ||
          event.code !== "Space" || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
      const target = event.target;
      if (target instanceof HTMLElement && target !== button &&
          (target.isContentEditable || target.closest("button, a[href], input, select, textarea, summary, [role=button]"))) return;
      event.preventDefault();
      if (!event.repeat) button.click();
    }
    window.addEventListener("keydown", advanceOnSpace);
    return () => window.removeEventListener("keydown", advanceOnSpace);
  }, []);

  function answer(played) {
    const submission = { attempt: card.attempt, dataset: card.dataset, played };
    run(async () => {
      if (played) setSubmitted({ key: attemptKey, move: played });
      localStorage.setItem("tempo.pending-answer.v2", JSON.stringify(submission));
      const result = await api("study/answer", submission);
      localStorage.removeItem("tempo.pending-answer.v2");
      if (!mounted.current) return;
      setCard((old) => ({ ...old, result }));
      invalidateCounts.current?.();
      if (result.display_move) {
        const board = new Chess(card.fen);
        const move = play(board, result.display_move);
        sounds.play(moveCue(move, board));
      }
      if (result.rating === "Good") setAdvanceAfter(card.attempt);
    });
  }

  const frame = openingPlaying ? frames[openingPly] : frames?.at(-1);
  const game = card && new Chess(openingPlaying ? frame.fen : card.fen);
  const movePrompt = game?.turn() === "w" ? "White to move" : "Black to move";
  const displayed = card?.result?.display_move;
  const awaitingResult = !card?.result && !openingPlaying && submitted?.key === attemptKey ? submitted?.move : undefined;
  const boardMove = displayed ?? awaitingResult;
  const played = displayed ? [displayed.slice(0, 2), displayed.slice(2, 4)] : undefined;
  const lastMove = boardMove ? [boardMove.slice(0, 2), boardMove.slice(2, 4)] : frame?.lastMove;
  const learner = card?.color === "white" ? "w" : "b";
  const previous = card?.kind === "opening"
    ? (frame?.movedBy !== learner ? frame?.lastMove : undefined)
    : (opponent?.key === attemptKey ? opponent?.move : undefined);
  const shapes = [...markedMove(previous, "red"), ...markedMove(played, "green")];
  if (boardMove) play(game, boardMove);
  return <section className="study" aria-label="Study">
    <div className="study-board">
      {card && <ChessBoard game={game} legal={openingPlaying ? undefined : card.legal} orientation={card.color}
        identity={`study-${card.attempt}-${epoch}`} locked={openingPlaying || pending || Boolean(error) || Boolean(card.result) || Boolean(awaitingResult)}
        lastMove={lastMove}
        shapes={shapes}
        promotionTarget={promotionTarget}
        onRejectedMove={() => sounds.play("illegal")}
        onMove={openingPlaying || card.result || awaitingResult ? undefined : answer} />}
    </div>
    <div className="study-panel">
      <h1>Study</h1>
      {counts && <p>{formatStudyCount(counts.new)} new · {formatStudyCount(counts.reviews)} {counts.reviews === 1 ? "review" : "reviews"}</p>}
      {error && <div className="error" role={error.code === "study_preparing" ? "status" : "alert"}>{error.message} <button onClick={() => run(retry.current)}>Retry</button></div>}
      {!loaded && !error && <p>Loading your deck…</p>}
      {loaded && !card && <><p>No cards are ready. Add an opening or tactic to get started.</p>
        <button onClick={() => next()} disabled={pending}>Check deck</button></>}
      {card && <>
        <h2>{card.result ? card.title : openingPlaying ? "Playing opening moves" : movePrompt}</h2>
        <div ref={setPromotionTarget} />
        <div className="controls">
          {card.result ? <>
            <strong role="status">{card.result.rating}</strong>
            <button ref={nextButton} className="primary" aria-keyshortcuts="Space" title="Next card (Space)" disabled={pending} onClick={() => next(card.attempt)}>Next card</button>
          </> : openingPlaying
            ? <button onClick={() => setOpeningProgress({ key: attemptKey, ply: lastPly })}>Skip to your move</button>
            : <button disabled={pending || Boolean(error)} onClick={() => answer(null)}>Reveal answer</button>}
        </div>
        {card.result && <div className="feedback">
          <p>Answer: {card.result.answers.map((move) => play(new Chess(card.fen), move).san).join(" or ")}</p>
          {(card.result.played_in_game || source?.played) &&
            <p>In the game you played {play(new Chess(card.fen), card.result.played_in_game || source.played).san}.</p>}
          {source?.game_id && <button onClick={() => openGame(source.game_id, source.ply)}>View source game</button>}
        </div>}
      </>}
    </div>
  </section>;
}
