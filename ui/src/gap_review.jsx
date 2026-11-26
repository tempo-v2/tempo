import React, { useEffect, useState } from "react";
import { Chess } from "chess.js";
import { api } from "./api.js";
import { ChessBoard } from "./board.jsx";

function evaluation(score) {
  if (score.kind === "mate") return `${score.winner === "white" ? "White" : "Black"} mates in ${score.moves}`;
  const value = (score.white / 100).toFixed(2);
  return score.white > 0 ? "+" + value : value;
}

function decodeReview(reply, report, index, fen) {
  const incomplete = new Error("The move review reply was incomplete.");
  if (reply?.report !== report || reply.gap !== index) throw incomplete;
  if (["conflict", "stale", "terminal", "busy", "unavailable"].includes(reply.status)) return reply;
  const notation = value => typeof value === "string" && value.length > 0 && value.length <= 32;
  if (reply.status === "covered" && notation(reply.san)) return reply;
  if (reply.status !== "ranked" || typeof reply.winning_mate !== "boolean" ||
      typeof reply.similarity_available !== "boolean" || !Array.isArray(reply.choices)) throw incomplete;
  const legal = new Set(new Chess(fen).moves({ verbose: true }).map(move => move.lan));
  function validMove(move) {
    const score = move?.score;
    return legal.has(move?.first_move) && notation(move.san) &&
      (score?.kind === "centipawns" ? Number.isSafeInteger(score.white) :
        score?.kind === "mate" && ["white", "black"].includes(score.winner) &&
        Number.isSafeInteger(score.moves) && score.moves > 0);
  }
  if (!validMove(reply.best) || reply.choices.length > legal.size) throw incomplete;
  const seen = new Set();
  for (const choice of reply.choices) {
    if (!validMove(choice) || seen.has(choice.first_move)) throw incomplete;
    seen.add(choice.first_move);
    if (choice.example === null && choice.distance === null) continue;
    if (!Number.isSafeInteger(choice.distance) || choice.distance < 0 ||
        typeof choice.example?.fen !== "string" || choice.example.fen.length > 256) throw incomplete;
    new Chess(choice.example.fen);
  }
  return reply;
}

export function GapReview({ report, index, fen, color, start, staged, canReview, canStage, canRescan, onStage, onRescan }) {
  const [retry, setRetry] = useState(0);
  const [review, setReview] = useState(null);
  const [selection, setSelection] = useState(null);
  const [example, setExample] = useState(false);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if ((!start && !retry) || document.hidden) return;
    const request = new AbortController();
    const signal = AbortSignal.any([request.signal, AbortSignal.timeout(6000)]);
    const hide = () => {
      if (document.hidden) {
        request.abort();
        setLoading(false);
      }
    };
    document.addEventListener("visibilitychange", hide);
    setReview(null);
    setSelection(null);
    setExample(false);
    setLoading(true);
    api("coverage/gaps/review", { report, gap: index }, signal, { requireJson: true })
      .then(reply => setReview(decodeReview(reply, report, index, fen)))
      .catch(() => {
        if (!request.signal.aborted) setReview({ status: "unavailable" });
      })
      .finally(() => {
        if (!request.signal.aborted) setLoading(false);
      });
    return () => {
      request.abort();
      document.removeEventListener("visibilitychange", hide);
    };
  }, [report, index, fen, start, retry]);

  const choices = review?.status === "ranked" ? review.choices : [];
  const selected = choices.find(row => row.first_move === (selection ?? staged)) ?? choices[0];
  const savedFen = example ? selected?.example?.fen : null;
  const arrow = selected?.first_move;
  const shapes = arrow ? [{ orig: arrow.slice(0, 2), dest: arrow.slice(2, 4), brush: "green" }] : [];
  const stale = ["covered", "stale", "conflict"].includes(review?.status);
  const unavailable = ["busy", "unavailable"].includes(review?.status);

  return <section aria-label="Review gap">
    <div className="similar-moves-board">
      <ChessBoard game={new Chess(savedFen ?? fen)} orientation={color}
        identity={savedFen ? "saved-gap-example" : "coverage-gap"} shapes={shapes} />
    </div>
    {loading && <p role="status">Reviewing moves…</p>}
    {!loading && (!review || unavailable) &&
      <button type="button" disabled={!canReview} onClick={() => setRetry(value => value + 1)}>{review ? "Retry review" : "Review moves"}</button>}
    {unavailable && <p>Move review is temporarily unavailable.</p>}
    {review?.status === "covered" && <p>Already covered · {review.san}</p>}
    {review?.status === "conflict" && <p>This position has an unresolved repertoire conflict.</p>}
    {review?.status === "stale" && <p>The repertoire has changed since this scan.</p>}
    {review?.status === "terminal" && <p>This position has no legal moves.</p>}
    {stale && <button type="button" disabled={!canRescan} onClick={onRescan}>Rescan coverage</button>}
    {review?.status === "ranked" && <>
      <p>{review.best.score.kind === "mate" && !review.winning_mate ? "Best defense" : "Stockfish best"} · {review.best.san} · {evaluation(review.best.score)}</p>
      {!choices.length && review.best.score.kind === "centipawns" && <p>No familiar moves within 30 CP.</p>}
      {!!choices.length && <>
        <div className="gap-review-table">
          <table aria-label="Candidate replies">
            <thead><tr><th scope="col">Move</th><th scope="col">Similarity distance</th><th scope="col">Stockfish eval (White)</th></tr></thead>
            <tbody>{choices.map((row, index) => <tr key={row.first_move}>
              <td><button type="button" aria-pressed={selected?.first_move === row.first_move}
                onClick={() => { setSelection(row.first_move); setExample(false); }}>
                {row.san}
              </button>{index === 0 && review.winning_mate && " · Recommended"}</td>
              <td>{row.distance ?? (review.similarity_available ? "No saved example" : "Unavailable")}</td>
              <td>{evaluation(row.score)}</td>
            </tr>)}</tbody>
          </table>
        </div>
        <div className="controls">
          {selected?.example && <button type="button" aria-pressed={!!savedFen} onClick={() => setExample(value => !value)}>
            {savedFen ? "Show gap position" : "Show closest saved position"}
          </button>}
          <button type="button" disabled={!canStage} onClick={() => onStage(selected.first_move)}>Use reply</button>
        </div>
      </>}
    </>}
  </section>;
}
