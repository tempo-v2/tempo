import { ExerciseLayout } from "./exercise_layout.jsx";
import { decodePuzzle, PuzzleBoard, recoverPuzzleActions } from "./puzzle_play.jsx";
import React, { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { Chess } from "chess.js";
import { api, readView } from "./api.js";
import {
  answerIntent, answerPayload, importPredecessorAnswer, isMove, nextSavedAnswer, resolveAnswer, saveAnswer,
} from "./answer-journal.js";
import { ChessBoard, play } from "./board.jsx";
import { moveCue } from "./sounds.js";

const successPauseMs = 800;
const openingPlyMs = 260;

function openingFrames(route, targetFen, learner) {
  if (!route?.moves?.length) return null;
  try {
    const game = new Chess(route.fen);
    const frames = [{ fen: game.fen() }];
    for (const uci of route.moves) {
      const move = play(game, uci);
      frames.push({ fen: game.fen(), lastMove: [move.from, move.to], movedBy: move.color,
        cue: moveCue(move, game, move.color === learner ? "move-self" : "move-opponent") });
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

const dailyOpeningOrder = {
  maia: "New openings use today’s Maia order.",
  saved: "New openings use the last saved Maia order.",
  standard: "New openings use Standard order.",
};

function useStudyCounts() {
  const [counts, setCounts] = useState(null);
  const invalidate = useRef(null);
  useEffect(() => {
    let active = true;
    let pending = null;
    let dirty = false;
    async function refresh() {
      if (!active || pending || document.visibilityState === "hidden") return;
      const request = new AbortController();
      pending = request;
      dirty = false;
      const timeout = setTimeout(() => request.abort(), 1000);
      try {
        const response = await fetch("/api/study/counts", { signal: request.signal });
        const totals = response.ok ? await response.json() : null;
        const valid = [totals?.new, totals?.reviews].every(validStudyCount) &&
          typeof totals.preparing === "boolean" && typeof totals.day === "string" &&
          /^\d{4}-\d{2}-\d{2}$/.test(totals.day) &&
          Object.hasOwn(dailyOpeningOrder, totals.opening_order);
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
    refresh();
    const timer = setInterval(refresh, 15_000);
    window.addEventListener("focus", refresh);
    document.addEventListener("visibilitychange", refresh);
    return () => {
      active = false;
      invalidate.current = null;
      pending?.abort();
      clearInterval(timer);
      window.removeEventListener("focus", refresh);
      document.removeEventListener("visibilitychange", refresh);
    };
  }, []);
  return [counts, invalidate];
}

function playableMoves(fen, incompleteReply) {
  try {
    return new Set(new Chess(fen).moves({ verbose: true }).map((move) => move.lan));
  } catch {
    throw new Error(incompleteReply);
  }
}

function decodeResult(body, incompleteReply, playable) {
  if (!["Good", "Again"].includes(body?.rating) ||
      !Array.isArray(body.answers) || !body.answers.length || !body.answers.every(isMove) ||
      (playable && body.answers.some((move) => !playable.has(move))) ||
      !(body.repeat_after === null || Number.isInteger(body.repeat_after) &&
        body.repeat_after >= 0 && body.repeat_after <= 255)) {
    throw new Error(incompleteReply);
  }
  return {
    ...body,
    display_move: body.answers.includes(body.display_move) ? body.display_move : body.answers[0],
    played_in_game: isMove(body.played_in_game) ? body.played_in_game : null,
  };
}

function decodeStudyReply(body, dataset) {
  if (body === null) return null;
  const incomplete = "The Study reply was incomplete. Retry.";
  if (!Number.isSafeInteger(body?.attempt) || body.attempt <= 0 ||
      typeof body.dataset !== "string" || !body.dataset || (dataset && body.dataset !== dataset) ||
      !["opening", "tactic"].includes(body.kind) || !["white", "black"].includes(body.color) ||
      !["single_answer", "full_line"].includes(body.activity_mode) ||
      typeof body.fen !== "string" || !body.fen ||
      !Array.isArray(body.legal) || !body.legal.every(isMove)) {
    throw new Error(incomplete);
  }
  // The server supplies the allowed moves. Verify that the board can render
  // those moves and the feedback from this position before replacing the card.
  const playable = playableMoves(body.fen, incomplete);
  const fullLine = body.activity_mode === "full_line";
  if (fullLine ? body.result !== null : body.puzzle !== undefined) throw new Error(incomplete);
  const puzzle = fullLine ? decodePuzzle(body.puzzle, "study", { dataset: body.dataset }) : null;
  const result = body.result === null ? null : decodeResult(body.result, incomplete, playable);
  const introduction = body.introduction_move;
  if (introduction !== undefined &&
      (fullLine || body.kind !== "opening" || result !== null || !isMove(introduction) ||
       !body.legal.includes(introduction) || !playable.has(introduction))) {
    throw new Error(incomplete);
  }
  if (body.legal.some((move) => !playable.has(move)) || (result === null && body.legal.length === 0)) {
    throw new Error(incomplete);
  }
  return {
    ...body,
    puzzle,
    title: typeof body.title === "string" ? body.title : "",
    result,
  };
}

// A typed terminal reply ends this exact intent. Transport failure keeps it pending.
async function deliverAnswer(intent, fen) {
  let result;
  try {
    const body = await api("study/answer", answerPayload(intent));
    const incomplete = "The Answer reply was incomplete. Retry.";
    // A mounted answer renders here. Startup replay renders from Current instead.
    const playable = fen === undefined ? undefined : playableMoves(fen, incomplete);
    result = decodeResult(body, incomplete, playable);
  } catch (error) {
    if (error.code === "refresh_required") {
      // The server rejected this exact legacy action before grading.
      await resolveAnswer(intent);
      return null;
    }
    if (!["stale_dataset", "stale_attempt", "answer_conflict"].includes(error.code)) throw error;
  }
  // A failed journal cleanup can cause an exact replay. Acceptance stays authoritative.
  void resolveAnswer(intent).catch(() => {});
  return result;
}

export function Study({ openGame, sounds, overlayOpen = false }) {
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
  const currentRead = useRef(null);
  async function loadCurrent() {
    currentRead.current?.abort();
    const request = new AbortController();
    currentRead.current = request;
    try {
      return decodeStudyReply(await readView("study", request.signal));
    } finally {
      if (currentRead.current === request) currentRead.current = null;
    }
  }
  const [promotionTarget, setPromotionTarget] = useState(null);
  const [epoch, setEpoch] = useState(0);
  const [advanceAfter, setAdvanceAfter] = useState(null);
  const mounted = useRef(true);
  const [openingProgress, setOpeningProgress] = useState(null);
  const attemptKey = card ? `${card.dataset}/${card.attempt}` : null;
  useLayoutEffect(() => sounds.cancel, [sounds, attemptKey]);
  const source = sourceRead?.key === attemptKey ? sourceRead.result : null;
  const learner = card?.color?.[0];
  const frames = useMemo(() => openingFrames(card?.lead_in, card?.fen, learner), [card?.lead_in, card?.fen, learner]);
  const openingPly = openingProgress?.key === attemptKey ? openingProgress.ply : 0;
  const lastPly = frames ? frames.length - 1 : 0;
  const openingPlaying = Boolean(frames && !card?.result && openingPly < lastPly);

  useEffect(() => {
    if (!openingPlaying) return;
    const timer = setTimeout(() => {
      sounds.play(frames[openingPly + 1].cue);
      setOpeningProgress((current) => current?.key === attemptKey && current.ply > openingPly
        ? current : { key: attemptKey, ply: openingPly + 1 });
    }, openingPlyMs);
    return () => clearTimeout(timer);
  }, [attemptKey, frames, openingPlaying, openingPly, sounds]);
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
  function showCard(view) {
    setCard(view);
    setLoaded(true);
    invalidateCounts.current?.();
  }
  async function loadNext(payload) {
    const body = await api("study/next", payload, undefined, { requireJson: true });
    const view = decodeStudyReply(body, payload.dataset);
    if (mounted.current) showCard(view);
  }
  function next(after = null) {
    setAdvanceAfter(null);
    const payload = { after, dataset: card?.dataset };
    return run(() => loadNext(payload));
  }
  useEffect(() => {
    mounted.current = true;
    run(async () => {
      await importPredecessorAnswer();
      let intent;
      while ((intent = await nextSavedAnswer(intent))) {
        await deliverAnswer(intent);
      }
      await recoverPuzzleActions("study");
      if (!mounted.current) return;
      async function resumeCurrent() {
        const active = await loadCurrent();
        if (!mounted.current) return;
        if (active !== null) {
          showCard(active);
          return;
        }
        const payload = { after: null };
        const initialNext = () => loadNext(payload);
        retry.current = initialNext;
        await initialNext();
      }
      retry.current = resumeCurrent;
      await resumeCurrent();
    });
    return () => {
      mounted.current = false;
      currentRead.current?.abort();
    };
  }, []);
  useEffect(() => {
    if (!attemptKey || card.kind === "opening" || card.activity_mode === "full_line") return;
    const request = new AbortController();
    opponentRead.current = readView(`study/${card.attempt}/opponent-move`, request.signal)
      .then((move) => {
        if (!request.signal.aborted && move) {
          setOpponent({ key: attemptKey, move: [move.slice(0, 2), move.slice(2, 4)] });
        }
      }).catch(() => {});
    return () => {
      request.abort();
      opponentRead.current = null;
    };
  }, [attemptKey, card?.kind, card?.activity_mode]);

  useEffect(() => {
    setSource(null);
    if (!card?.result) return;
    const request = new AbortController();
    Promise.resolve(opponentRead.current).then(async () => {
      if (request.signal.aborted) return;
      const result = await readView(`study/${card.attempt}/context`, request.signal);
      if (!request.signal.aborted) setSource({ key: attemptKey, result });
    }).catch(() => {});
    return () => request.abort();
  }, [attemptKey, Boolean(card?.result)]);

  useEffect(() => {
    if (advanceAfter === null || overlayOpen) return;
    const timer = setTimeout(() => next(advanceAfter), successPauseMs);
    return () => clearTimeout(timer);
  }, [advanceAfter, overlayOpen]);

  useEffect(() => {
    function advanceOnSpace(event) {
      const button = nextButton.current;
      if (overlayOpen || !button || button.disabled || event.defaultPrevented || event.isComposing ||
          event.code !== "Space" || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
      const target = event.target;
      if (target instanceof HTMLElement && target !== button &&
          (target.isContentEditable || target.closest("button, a[href], input, select, textarea, summary, [role=button]"))) return;
      event.preventDefault();
      if (!event.repeat) button.click();
    }
    window.addEventListener("keydown", advanceOnSpace);
    return () => window.removeEventListener("keydown", advanceOnSpace);
  }, [overlayOpen]);

  function answer(played) {
    if (waiting.current) return;
    const intent = answerIntent({ attempt: card.attempt, dataset: card.dataset, played });
    if (played) {
      const board = new Chess(card.fen);
      sounds.play(moveCue(play(board, played), board));
    }
    run(async () => {
      // Render the drop in the same update that locks the board.
      if (played) setSubmitted({ key: attemptKey, move: played });
      await saveAnswer(intent);
      const result = await deliverAnswer(intent, card.fen);
      if (!mounted.current) return;
      if (!result) {
        setSubmitted(null);
        async function refreshCurrent() {
          const current = await loadCurrent();
          if (!mounted.current) return;
          setCard(current);
          invalidateCounts.current?.();
        }
        retry.current = refreshCurrent;
        await refreshCurrent();
        return;
      }
      setCard((old) => ({ ...old, result }));
      invalidateCounts.current?.();
      if (result.display_move && result.display_move !== played) {
        const board = new Chess(card.fen);
        const move = play(board, result.display_move);
        sounds.play(moveCue(move, board));
      }
      if (result.rating === "Good") setAdvanceAfter(card.attempt);
    });
  }

  const heading = <>
    <h1>Study</h1>
    {counts && <>
      <p>{formatStudyCount(counts.new)} new · {formatStudyCount(counts.reviews)} {counts.reviews === 1 ? "review" : "reviews"}</p>
      <small>{counts.preparing ? "Preparing today’s queue…" : dailyOpeningOrder[counts.opening_order]}</small>
    </>}
  </>;
  if (card?.activity_mode === "full_line") return <PuzzleBoard key={card.puzzle.play} view={card.puzzle} scope="study" sounds={sounds}
    onView={(puzzle) => showCard({ ...card, puzzle })}
    onRefresh={async () => {
      const current = await loadCurrent();
      if (mounted.current) showCard(current);
    }}
    heading={heading}
    continuation={<button ref={nextButton} disabled={pending} onClick={() => next(card.attempt)}>Next</button>}>
    {error && <div role="alert">{error.message} <button disabled={pending} onClick={() => run(retry.current)}>Retry</button></div>}
  </PuzzleBoard>;

  const frame = openingPlaying ? frames[openingPly] : frames?.at(-1);
  const game = card && new Chess(openingPlaying ? frame.fen : card.fen);
  const movePrompt = game?.turn() === "w" ? "White to move" : "Black to move";
  const displayed = card?.result?.display_move;
  const awaitingResult = !card?.result && !openingPlaying && submitted?.key === attemptKey ? submitted?.move : undefined;
  const boardMove = displayed ?? awaitingResult;
  const played = displayed ? [displayed.slice(0, 2), displayed.slice(2, 4)] : undefined;
  const lastMove = boardMove ? [boardMove.slice(0, 2), boardMove.slice(2, 4)] : frame?.lastMove;
  const previous = card?.kind === "opening"
    ? (frame?.movedBy !== learner ? frame?.lastMove : undefined)
    : (opponent?.key === attemptKey ? opponent?.move : undefined);
  let playedInGame;
  const gameMove = card?.result?.played_in_game ?? source?.played;
  if (isMove(gameMove)) {
    try {
      playedInGame = play(new Chess(card.fen), gameMove).san;
    } catch { /* Optional context can be unavailable for the issued position. */ }
  }
  const introduction = !openingPlaying && !card?.result && !awaitingResult && !pending && !error
    ? card?.introduction_move : undefined;
  const shapes = [...markedMove(previous, "red"), ...markedMove(played, "green")];
  if (introduction) shapes.push({ orig: introduction.slice(0, 2), dest: introduction.slice(2, 4), brush: "yellow" });
  const introductionPrompt = introduction && `Play ${play(new Chess(card.fen), introduction).san}`;
  if (boardMove) play(game, boardMove);
  const board = card && <ChessBoard game={game} legal={openingPlaying ? undefined : card.legal} orientation={card.color}
    identity={`study-${card.attempt}-${epoch}`} locked={openingPlaying || pending || Boolean(error) || Boolean(card.result) || Boolean(awaitingResult)}
    lastMove={lastMove}
    shapes={shapes}
    promotionTarget={promotionTarget}
    onRejectedMove={() => sounds.play("illegal")}
    onMove={openingPlaying || card.result || awaitingResult ? undefined : answer} />;
  return <ExerciseLayout label="Study" board={board}>
    {heading}
    {error && <div className="error" role={error.code === "study_preparing" ? "status" : "alert"}>{error.message} <button onClick={() => run(retry.current)}>Retry</button></div>}
    {!loaded && !error && <p>Loading your deck…</p>}
    {loaded && !card && !error && <><p>No cards are ready in today’s queue.</p>
      <button onClick={() => next()} disabled={pending}>Check deck</button></>}
    {card && <>
      <h2>{card.result ? card.title : openingPlaying ? "Playing opening moves" : introductionPrompt ?? movePrompt}</h2>
      <div ref={setPromotionTarget} />
      <div className="controls">
        {card.result ? <>
          <strong role="status">{card.result.rating}</strong>
          <button ref={nextButton} className="primary" aria-keyshortcuts="Space" title="Next card (Space)" disabled={pending} onClick={() => next(card.attempt)}>Next card</button>
        </> : openingPlaying
          ? <button onClick={() => {
            sounds.cancel();
            setOpeningProgress({ key: attemptKey, ply: lastPly });
          }}>Skip to your move</button>
          : <button disabled={pending || Boolean(error)} onClick={() => answer(null)}>Reveal answer</button>}
      </div>
      {card.result && <div className="feedback">
        <p>Answer: {card.result.answers.map((move) => play(new Chess(card.fen), move).san).join(" or ")}</p>
        {playedInGame && <p>In the game you played {playedInGame}.</p>}
        {source?.game_id && <button onClick={() => openGame(source.game_id, source.ply)}>View source game</button>}
      </div>}
    </>}
  </ExerciseLayout>;
}
