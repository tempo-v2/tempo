import React, { useEffect, useRef, useState } from "react";
import { Chess } from "chess.js";
import { ExerciseLayout } from "./exercise_layout.jsx";
import { api } from "./api.js";
import { ChessBoard, Replay, position } from "./board.jsx";
import { isMove } from "./answer-journal.js";

const endpoint = {
  tactics: "tactics/plays/move",
  study: "study/puzzle-move",
};
const key = (scope) => `tempo.lichess.v1.${scope}`;
const terminalErrors = ["stale_attempt", "stale_dataset", "answer_conflict", "illegal_move", "pack_changed"];

export function decodePuzzle(value, scope, intent) {
  const failed = "The puzzle reply was incomplete. Retry.";
  if (!value || !Number.isSafeInteger(value.play) || value.play < 1 ||
      !Number.isSafeInteger(value.step) || value.step < 0 ||
      typeof value.puzzle !== "string" || typeof value.dataset !== "string" ||
      !(scope === "study" ? value.mode === "study" : ["first", "practice"].includes(value.mode)) ||
      !["white", "black"].includes(value.color) || typeof value.fen !== "string" ||
      !Array.isArray(value.legal) || !value.legal.every(isMove) ||
      !(value.last_move === null || isMove(value.last_move)) ||
      (intent && (intent.dataset !== value.dataset ||
        (intent.play !== undefined && intent.play !== value.play) ||
        (intent.puzzle !== undefined && intent.puzzle !== value.puzzle) ||
        (intent.step !== undefined && (value.step < intent.step || value.result === null && value.step === intent.step))))) throw new Error(failed);
  if (value.result !== null && (!["Good", "Again"].includes(value.result?.rating) ||
      !Array.isArray(value.solution?.moves) || !value.solution.moves.length ||
      !value.solution.moves.every(isMove) || typeof value.solution.fen !== "string")) throw new Error(failed);
  if (value.result === null && (value.solution !== null || !value.legal.length)) throw new Error(failed);
  const board = new Chess(value.fen);
  const playable = new Set(board.moves({ verbose: true }).map(({ from, to, promotion }) => from + to + (promotion ?? "")));
  if (value.legal.some((move) => !playable.has(move))) throw new Error(failed);
  if (value.result !== null) position(value.solution);
  return value;
}

async function deliver(scope, record) {
  const storageKey = key(scope);
  const snapshot = JSON.stringify(record);
  sessionStorage.setItem(storageKey, snapshot);
  try {
    const url = record.kind === "start" ? "tactics/plays" : endpoint[scope];
    return decodePuzzle(await api(url, record.intent, undefined, { requireJson: true }), scope, record.intent);
  } catch (error) {
    if (!terminalErrors.includes(error.code)) throw error;
    return null;
  }
}

async function finish(scope, record) {
  const reply = await deliver(scope, record);
  if (sessionStorage.getItem(key(scope)) === JSON.stringify(record)) sessionStorage.removeItem(key(scope));
  return reply;
}

export function recoverPuzzleAction(scope) {
  const pending = sessionStorage.getItem(key(scope));
  return pending ? finish(scope, JSON.parse(pending)) : Promise.resolve(null);
}

export async function startPuzzle(intent) {
  return finish("tactics", { kind: "start", intent });
}

export function PuzzleBoard({ view, scope, sounds, onView, onRefresh, continuation, heading, children }) {
  const [pending, setPending] = useState(false);
  const [issue, setIssue] = useState("");
  const [replay, setReplay] = useState(null);
  const [promotionTarget, setPromotionTarget] = useState(null);
  const busy = useRef(false);
  const retry = useRef(null);
  const mounted = useRef(false);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  async function run(work) {
    if (busy.current) return;
    busy.current = true;
    retry.current = work;
    setPending(true);
    setIssue("");
    try {
      await work();
      retry.current = null;
    } catch (error) {
      if (mounted.current) setIssue(error.message);
    } finally {
      busy.current = false;
      if (mounted.current) setPending(false);
    }
  }
  function move(played) {
    const intent = { dataset: view.dataset, play: view.play, step: view.step, played };
    void run(async () => {
      const next = await finish(scope, { kind: "move", intent });
      if (!mounted.current) return;
      if (next) {
        if (next.result?.rating === "Again") sounds.play("illegal");
        onView(next);
      } else {
        // A concurrent tab may already have advanced. Read the authoritative position.
        retry.current = onRefresh;
        await onRefresh();
      }
    });
  }
  const finished = view.result !== null;
  const lastMove = view.last_move ? [view.last_move.slice(0, 2), view.last_move.slice(2, 4)] : undefined;
  function layout(board, replayControls) {
    return <ExerciseLayout label={scope === "study" ? "Study" : "Play puzzle"} board={board}>
      {heading}
      <div ref={setPromotionTarget} />
      <div className="controls">
        {finished ? <>
          <strong role="status">{view.result.rating === "Good" ? "Solved" : "Again"}</strong>
          {view.result.due_day && <span>First Study review {view.result.due_day}</span>}
          {view.result.repeat === "Deferred" && <span>Repeat saved for Study</span>}
          {view.result.repeat?.Placed !== undefined && <span>Repeat after {view.result.repeat.Placed} cards</span>}
          <button onClick={() => setReplay(replay === null ? view.step : null)}>{replay === null ? "Show solution" : "Back to result"}</button>
          {continuation}
        </> : <button disabled={pending || Boolean(issue)} onClick={() => move(null)}>Reveal solution</button>}
        {issue && <span role="alert">{issue} <button disabled={pending} onClick={() => run(retry.current)}>Retry</button></span>}
      </div>
      {replayControls}
      {children}
    </ExerciseLayout>;
  }
  return replay === null
    ? layout(<ChessBoard game={new Chess(view.fen)} orientation={view.color}
        legal={view.legal} identity={`${scope}/${view.dataset}/${view.play}/${view.step}`}
        lastMove={lastMove} promotionTarget={promotionTarget}
        onRejectedMove={() => sounds.play("illegal")}
        onMove={finished ? undefined : move} locked={pending || Boolean(issue)} />)
    : <Replay route={view.solution} color={view.color} ply={replay} onPly={setReplay} sounds={sounds} layout={layout} />;
}
