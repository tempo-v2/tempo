import React, { useEffect, useRef, useState } from "react";
import { Chess } from "chess.js";
import { api, visibleRead } from "./api.js";
import { ReplacementImpact } from "./replacement_impact.jsx";

const endpoint = "openings/study-review";
const journalKey = "tempo-lean-study-opening-edit-v1";
const actionKey = ({ kind, movement }) => JSON.stringify([kind, movement ?? null]);
const addressKey = ({ dataset, attempt }) => JSON.stringify([dataset, attempt]);
const basisKey = ({ address, repertoire, position, version, reply_revision, line_revision }) =>
  JSON.stringify([addressKey(address), repertoire, position, version, reply_revision, line_revision]);

function sameEdit(a, b) {
  return a && b && basisKey(a.basis) === basisKey(b.basis) && actionKey(a.action) === actionKey(b.action);
}

function storedEdit() {
  try { return JSON.parse(localStorage.getItem(journalKey)); } catch { return null; }
}

const terminal = new Set(["stale_study_move", "replacement_over_budget",
  "replacement_preview_unavailable", "opening_reply_conflict"]);

// The saved envelope is separate from Answer recovery and from the review panel.
export function useOpeningEdit(onSaved, enabled) {
  const [entry, setEntry] = useState(() => enabled ? storedEdit() : null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const inFlight = useRef(false);
  useEffect(() => {
    if (!enabled) return;
    const refresh = event => {
      if (event.key !== journalKey) return;
      const current = storedEdit();
      setEntry(current);
      if (current?.state === "accepted") onSaved();
    };
    window.addEventListener("storage", refresh);
    return () => window.removeEventListener("storage", refresh);
  }, [enabled, onSaved]);

  async function deliver(payload) {
    if (inFlight.current) return;
    inFlight.current = true;
    setBusy(true);
    setError(null);
    let accepted = false;
    try {
      const claimed = await navigator.locks.request(journalKey, () => {
        const current = storedEdit();
        if (current?.payload.command === payload.command) return current;
        if (current?.state === "pending") return null;
        const next = { state: "pending", payload };
        localStorage.setItem(journalKey, JSON.stringify(next));
        setEntry(next);
        return next;
      });
      if (!claimed) throw new Error("Resolve the pending repertoire save first.");
      if (claimed.state === "accepted") {
        setEntry(claimed);
        return;
      }
      let receipt;
      try {
        receipt = await api(endpoint + "/apply", payload, undefined, { requireJson: true });
        if (receipt?.saved !== true || receipt.command !== payload.command || !sameEdit(receipt.edit, payload.edit)) {
          throw new Error("The repertoire save reply was incomplete.");
        }
        accepted = true;
      } catch (failure) {
        if (terminal.has(failure.code)) {
          await navigator.locks.request(journalKey, () => {
            const current = storedEdit();
            if (current?.state === "pending" && current.payload.command === payload.command) {
              localStorage.removeItem(journalKey);
              setEntry(null);
            }
          });
        }
        throw failure;
      }
      await navigator.locks.request(journalKey, () => {
        const current = storedEdit();
        if (current?.payload.command === payload.command) {
          const next = { state: "accepted", payload };
          setEntry(next);
          localStorage.setItem(journalKey, JSON.stringify(next));
        }
      });
    } catch (failure) {
      if (!accepted) setError({ basis: payload.edit.basis, command: payload.command, failure });
    } finally {
      if (accepted) onSaved();
      inFlight.current = false;
      setBusy(false);
    }
  }

  return { entry, busy, error, deliver };
}

function editFailure({ entry, error }, address) {
  if (!error) return null;
  if (entry?.state === "pending") {
    return entry.payload.command === error.command ? error : null;
  }
  if (entry?.state === "accepted" && entry.payload.command === error.command) return null;
  return address && addressKey(error.basis.address) === addressKey(address) ? error : null;
}

export function OpeningEditStatus({ save, card }) {
  const { entry, busy, deliver } = save;
  const error = editFailure(save, card);
  const pending = entry?.state === "pending";
  const accepted = entry?.state === "accepted" && card &&
    addressKey(entry.payload.edit.basis.address) === addressKey(card);
  return <>
    {pending && <div className="controls">
      <span role="status">{busy ? "Saving repertoire change…" : "Repertoire save pending."}</span>
      {!busy && <button onClick={() => deliver(entry.payload)}>Retry same repertoire save</button>}
    </div>}
    {error && <p className="error" role="alert">{error.failure.message}</p>}
    {accepted && <p role="status">{entry.payload.edit.action.kind === "remove"
      ? "Move removed from the repertoire." : "Replacement saved to the repertoire."}</p>}
  </>;
}

function useReviewRead(path, payload, timeout, decode) {
  const [retry, setRetry] = useState(0);
  const [state, setState] = useState(null);
  useEffect(() => {
    if (!payload) return;
    setState(null);
    return visibleRead(async (signal) => {
      try {
        const value = await api(path, payload, AbortSignal.any([signal, AbortSignal.timeout(timeout)]), { requireJson: true });
        if (!signal.aborted) setState({ payload, value: decode(value) });
      } catch (error) {
        if (!signal.aborted) setState({ payload, error });
      }
    });
  }, [path, payload, retry, timeout, decode]);
  return [state?.payload === payload ? state : null, () => setRetry(n => n + 1)];
}

function decodeReview(value, address, card) {
  if (addressKey(value.basis.address) !== addressKey(address) ||
      value.fen !== card.fen || !card.result.answers.includes(value.issued_move) ||
      !card.legal.includes(value.issued_move) ||
      typeof value.repertoire !== "string" || (value.route !== null && typeof value.route !== "string") ||
      !["available", "route_unavailable"].includes(value.editing)) {
    throw new Error("The saved move reply was incomplete.");
  }
  return value;
}

function validScore(score) {
  return score?.kind === "centipawns" ? Number.isSafeInteger(score.white) :
    score?.kind === "mate" && ["white", "black"].includes(score.winner) &&
    Number.isSafeInteger(score.moves) && score.moves > 0;
}

function decodeEvidence(value, payload, fen, legal) {
  const invalid = new Error("The move evidence reply was incomplete.");
  if (value.source !== payload.source || basisKey(value.basis) !== basisKey(payload.basis)) throw invalid;
  const evidence = value.evidence;
  if (["busy", "unavailable"].includes(evidence?.status)) return evidence;
  if (evidence?.status !== "ready") throw invalid;
  if (value.source === "engine") {
    const roots = new Set(evidence.lines.map(line => line.first_move));
    if (evidence.depth !== 10 || typeof evidence.engine !== "string" ||
        evidence.lines.length !== legal.size || roots.size !== legal.size ||
        evidence.lines.some(line => !legal.has(line.first_move) || !validScore(line.score))) throw invalid;
  } else {
    const seen = new Set();
    for (const candidate of evidence.candidates) {
      const example = new Chess(candidate.example.fen);
      if (!legal.has(candidate.first_move) || seen.has(candidate.first_move) ||
          !Number.isSafeInteger(candidate.distance) || candidate.distance < 0 ||
          example.fen().split(" ").slice(0, 4).join(" ") === new Chess(fen).fen().split(" ").slice(0, 4).join(" ") ||
          !example.moves({ verbose: true }).some(move => move.lan === candidate.first_move)) throw invalid;
      seen.add(candidate.first_move);
    }
  }
  return evidence;
}

function evaluation(score) {
  if (score.kind === "mate") return `${score.winner === "white" ? "White" : "Black"} mates in ${score.moves}`;
  const value = (score.white / 100).toFixed(2);
  return score.white > 0 ? "+" + value : value;
}

function EvidenceStatus({ name, state, retry }) {
  if (!state) return <p role="status">Loading {name}…</p>;
  if (state.value?.status === "ready") return null;
  return <div className="controls"><span>{name} unavailable.</span>
    <button onClick={retry}>Retry {name}</button></div>;
}

export function StudyMoveReview({ card, boardPosition, save, onClose }) {
  const [selected, setSelected] = useState(card.result.answers[0]);
  const [example, setExample] = useState(false);
  const [impact, setImpact] = useState(null);
  const [previewError, setPreviewError] = useState(null);
  const [preparing, setPreparing] = useState(false);
  const previewRequest = useRef(null);
  const heading = useRef(null);
  const address = React.useMemo(() => ({ dataset: card.dataset, attempt: card.attempt }), [card.dataset, card.attempt]);
  const legal = React.useMemo(() => new Map(new Chess(card.fen).moves({ verbose: true })
    .filter(move => card.legal.includes(move.lan)).map(move => [move.lan, move.san])), [card.fen, card.legal]);
  const decode = React.useCallback(value => decodeReview(value, address, card), [address, card]);
  const [review, retryReview] = useReviewRead(endpoint, address, 1000, decode);
  const view = review?.value;
  const engineInput = React.useMemo(() => view ? { basis: view.basis, source: "engine" } : null, [view]);
  const similarityInput = React.useMemo(() => view ? { basis: view.basis, source: "similarity" } : null, [view]);
  const decodeEngine = React.useCallback(value => decodeEvidence(value, engineInput, card.fen, legal), [engineInput, card.fen, legal]);
  const decodeSimilarity = React.useCallback(value => decodeEvidence(value, similarityInput, card.fen, legal), [similarityInput, card.fen, legal]);
  const [engine, retryEngine] = useReviewRead(endpoint + "/evidence", engineInput, 6500, decodeEngine);
  const [similarity, retrySimilarity] = useReviewRead(endpoint + "/evidence", similarityInput, 1500, decodeSimilarity);
  const scores = engine?.value?.lines;
  const closest = similarity?.value?.candidates?.find(row => row.first_move === selected);
  const score = scores?.find(line => line.first_move === selected)?.score;
  const best = scores?.[0];
  const loss = score?.kind === "centipawns" && best?.score.kind === "centipawns"
    ? (best.score.white - score.white) * (card.color === "white" ? 1 : -1) : null;
  const accepted = save.entry?.state === "accepted" &&
    addressKey(save.entry.payload.edit.basis.address) === addressKey(address);
  const blocked = save.busy || save.entry?.state === "pending" || accepted;
  const error = editFailure(save, address);
  const failedSave = view && error && basisKey(error.basis) === basisKey(view.basis)
    ? error.failure : null;
  const stale = [engine?.error, similarity?.error, previewError, failedSave]
    .some(error => error?.code === "stale_study_move");
  const boardFen = example && closest ? closest.example.fen : card.fen;

  useEffect(() => {
    heading.current?.focus();
    return () => previewRequest.current?.abort();
  }, []);
  useEffect(() => { boardPosition({ fen: boardFen, move: selected }); }, [boardPosition, boardFen, selected]);

  async function preview(action) {
    previewRequest.current?.abort();
    const request = new AbortController();
    previewRequest.current = request;
    setImpact(null);
    setPreviewError(null);
    setPreparing(true);
    const edit = { basis: view.basis, action };
    try {
      const next = await api(endpoint + "/preview", edit,
        AbortSignal.any([request.signal, AbortSignal.timeout(12_000)]), { requireJson: true });
      if (!sameEdit(next.edit, edit) || typeof next.command !== "string" || !next.command ||
          typeof next.over_budget !== "boolean" || !Array.isArray(next.impact) ||
          !Number.isSafeInteger(next.affected) || next.affected < next.impact.length ||
          (!next.over_budget && next.affected !== next.impact.length) ||
          next.impact.some(line => typeof line.name !== "string" || typeof line.before !== "string" ||
            (line.after !== null && typeof line.after !== "string"))) {
        throw new Error("The repertoire preview reply was incomplete.");
      }
      if (!request.signal.aborted) setImpact(next);
    } catch (error) {
      if (!request.signal.aborted) setPreviewError(error);
    } finally {
      if (!request.signal.aborted) setPreparing(false);
    }
  }

  function pick(move) {
    previewRequest.current?.abort();
    setPreparing(false);
    setPreviewError(null);
    setImpact(null);
    setSelected(move);
    setExample(false);
  }

  const moves = scores?.map(line => line.first_move) ??
    [...legal.keys()].sort((a, b) => legal.get(a).localeCompare(legal.get(b)));
  return <section className="study-move-review" aria-label="Review repertoire move">
    <div className="controls">
      <h2 ref={heading} tabIndex={-1}>Review repertoire move</h2>
      <button onClick={onClose}>Close review</button>
    </div>
    {!review && !accepted && <p role="status">Loading saved move…</p>}
    {review?.error && !accepted && <>
      <p className="error" role="alert">{review.error.message}</p>
      <button onClick={retryReview}>Reload saved move</button>
    </>}
    {view && !accepted && <>
      <p><strong>{view.editing === "available" ? "Saved reply" : "Study answer"} {legal.get(view.issued_move)}</strong> · {view.repertoire}</p>
      {view.route ? <p>{view.route}</p> : <p>A current saved route could not be verified.</p>}
      <small>Move authorship unknown</small>
      <EvidenceStatus name="Stockfish" state={engine} retry={retryEngine} />
      <EvidenceStatus name="similar saved positions" state={similarity} retry={retrySimilarity} />
      {best && <p>Stockfish depth {engine.value.depth} · Best {legal.get(best.first_move)} ({evaluation(best.score)})</p>}
      <label>Compare a legal move
        <select value={selected} onChange={event => pick(event.target.value)}>
          {moves.map(move => <option key={move} value={move}>{legal.get(move)}{move === view.issued_move ? (view.editing === "available" ? " (saved)" : " (studied)") : ""}</option>)}
        </select>
      </label>
      {score && <p>{legal.get(selected)} · {evaluation(score)} (White){loss !== null && ` · ${loss} CP below best`}</p>}
      {closest ? <div className="controls">
        <span>Closest other saved position · Distance {closest.distance}</span>
        <button aria-pressed={!!example} onClick={() => setExample(!example)}>
          {example ? "Show Study position" : "Show closest saved position"}
        </button>
      </div> : similarity?.value?.status === "ready" && <p>No other saved position for this move.</p>}
      {stale && <p role="status">The saved reply or repertoire changed. Reopen the review to load current evidence.</p>}
      <div className="controls">
        <button disabled={blocked || stale || view.editing !== "available" || preparing || selected === view.issued_move}
          onClick={() => preview({ kind: "replace", movement: selected })}>Preview replacement</button>
        <button disabled={blocked || stale || view.editing !== "available" || preparing}
          onClick={() => preview({ kind: "remove" })}>Preview removal</button>
      </div>
      {preparing && <p role="status">Checking saved continuations…</p>}
      {previewError && <p className="error" role="alert">{previewError.message}</p>}
      {impact && !stale && <section className="gap-confirmation" aria-label="Saved line changes">
        <h3>{impact.edit.action.kind === "remove"
          ? `Remove ${legal.get(view.issued_move)}`
          : `Replace ${legal.get(view.issued_move)} with ${legal.get(impact.edit.action.movement)}`}</h3>
        <p>{view.repertoire} · {impact.affected} affected {impact.affected === 1 ? "line" : "lines"}</p>
        <ReplacementImpact impact={impact.impact} />
        {impact.over_budget ? <p>This change exceeds the atomic save limit.</p> :
          <button className="primary" disabled={blocked || preparing}
            onClick={() => save.deliver({ edit: impact.edit, command: impact.command })}>
            {impact.edit.action.kind === "remove" ? "Confirm removal" : "Confirm replacement"}
          </button>}
      </section>}
    </>}
  </section>;
}
