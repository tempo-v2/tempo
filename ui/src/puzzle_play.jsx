import React, { useEffect, useRef, useState } from "react";
import { Chess } from "chess.js";
import { ExerciseLayout } from "./exercise_layout.jsx";
import { api } from "./api.js";
import { ChessBoard, Replay, position } from "./board.jsx";
import { isMove } from "./answer-journal.js";
import { importPuzzleCommand, nextPuzzleCommand, resolvePuzzleCommand, savePuzzleCommand } from "./puzzle-journal.js";

const endpoint = {
  tactics: "tactics/plays/move",
  study: "study/puzzle-move",
};
const terminalErrors = ["stale_attempt", "stale_dataset", "answer_conflict", "illegal_move", "pack_changed"];

export function decodePuzzle(value, scope, intent) {
  const failed = "The puzzle reply was incomplete. Retry.";
  if (!value || !Number.isSafeInteger(value.play) || value.play < 1 ||
      !Number.isSafeInteger(value.step) || value.step < 0 ||
      typeof value.puzzle !== "string" || typeof value.dataset !== "string" ||
      !(scope === "study" ? value.mode === "study" : ["first", "practice"].includes(value.mode)) ||
      !["white", "black"].includes(value.color) || typeof value.fen !== "string" ||
      !Array.isArray(value.legal) || !value.legal.every(isMove) ||
      !(value.step === 0 ? value.last_move === null : isMove(value.last_move)) ||
      (intent && (intent.dataset !== value.dataset ||
        (intent.play !== undefined && intent.play !== value.play) ||
        (intent.puzzle !== undefined && intent.puzzle !== value.puzzle) ||
        (intent.practice !== undefined && value.mode !== (intent.practice ? "practice" : "first"))))) throw new Error(failed);
  if (value.result !== null && (!["Good", "Again"].includes(value.result?.rating) ||
      !validRepeat(value.result.repeat) ||
      !(value.result.due_day === null || /^\d{4}-\d{2}-\d{2}$/.test(value.result.due_day)) ||
      !Array.isArray(value.solution?.moves) || !value.solution.moves.length ||
      !value.solution.moves.every(isMove) || typeof value.solution.fen !== "string")) throw new Error(failed);
  if (value.result === null) {
    if (value.solution !== null || !value.legal.length || value.step % 2 !== 0) throw new Error(failed);
  } else {
    const end = value.solution.moves.length;
    if (value.result.rating === "Good" ? value.step !== end : value.step >= end || value.step % 2 !== 0) {
      throw new Error(failed);
    }
  }
  if (intent?.step !== undefined && !matchesMoveReply(value, intent)) throw new Error(failed);
  const board = new Chess(value.fen);
  const playable = new Set(board.moves({ verbose: true }).map(({ from, to, promotion }) => from + to + (promotion ?? "")));
  if (value.legal.some((move) => !playable.has(move))) throw new Error(failed);
  if (value.result !== null) position(value.solution);
  return value;
}

// Move receipts project the single submitted action. Start can instead resume an existing play.
function matchesMoveReply(value, { step, played }) {
  if (played === null || value.result?.rating === "Again") {
    return value.result?.rating === "Again" && value.step === step &&
      (played === null || value.solution.moves[step] !== played);
  }
  if (value.result === null) return value.step === step + 2;
  return (value.step === step + 1 || value.step === step + 2) && value.solution.moves[step] === played;
}

function validRepeat(repeat) {
  return repeat === "None" || repeat === "Deferred" ||
    (Number.isInteger(repeat?.Placed) && repeat.Placed >= 0 && repeat.Placed <= 20);
}

async function deliver(scope, command) {
  let reply = null;
  try {
    const url = command.kind === "start" ? "tactics/plays" : endpoint[scope];
    const body = await api(url, command.intent, undefined, { requireJson: true });
    reply = decodePuzzle(body, scope, command.intent);
  } catch (error) {
    if (!terminalErrors.includes(error.code)) throw error;
  }
  // A cleanup failure can cause another exact replay. It cannot undo settlement.
  await resolvePuzzleCommand(scope, command).catch(() => {});
  return reply;
}

async function sendPuzzleAction(scope, command) {
  await savePuzzleCommand(scope, command);
  return deliver(scope, command);
}

export async function recoverPuzzleActions(scope, onAccepted) {
  await importPuzzleCommand(scope);
  let entry;
  while ((entry = await nextPuzzleCommand(scope, entry?.key))) {
    const reply = await deliver(scope, entry.command);
    if (reply) onAccepted?.(reply);
  }
}

export function startPuzzle(intent) {
  return sendPuzzleAction("tactics", { kind: "start", intent });
}

export function PuzzleBoard({ view, scope, sounds, onView, onRefresh, continuation, heading, children }) {
  const [pending, setPending] = useState(false);
  const [issue, setIssue] = useState("");
  const [replay, setReplay] = useState(null);
  const [promotionTarget, setPromotionTarget] = useState(null);
  const [replayTarget, setReplayTarget] = useState(null);
  const busy = useRef(false);
  const retry = useRef(null);
  const mounted = useRef(false);
  const currentView = useRef(view);
  currentView.current = view;
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
      const next = await sendPuzzleAction(scope, { kind: "move", intent });
      const current = currentView.current;
      if (!mounted.current || current.dataset !== intent.dataset || current.play !== intent.play ||
          current.step !== intent.step || current.result !== null) return;
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
  const board = replay === null
    ? <ChessBoard game={new Chess(view.fen)} orientation={view.color}
        legal={view.legal} identity={`${scope}/${view.dataset}/${view.play}/${view.step}`}
        lastMove={lastMove} promotionTarget={promotionTarget}
        onRejectedMove={() => sounds.play("illegal")}
        onMove={finished ? undefined : move} locked={pending || Boolean(issue)} />
    : <Replay route={view.solution} color={view.color} ply={replay} onPly={setReplay} sounds={sounds}
        controlsTarget={replayTarget} />;
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
    <div ref={setReplayTarget} />
    {children}
  </ExerciseLayout>;
}
