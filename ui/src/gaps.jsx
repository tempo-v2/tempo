import React, { useEffect, useRef, useState } from "react";
import { Chess } from "chess.js";
import { api, readView, visibleRead } from "./api.js";
import { play, position } from "./board.jsx";
import { GapReview } from "./gap_review.jsx";
import { repertoireKey, useRepertoirePages } from "./repertoire_pages.js";

const storage = "tempo-lean-gap-draft-v1";
function stored(key) {
  try { return JSON.parse(localStorage.getItem(key)); } catch { return null; }
}
function restore() {
  return {
    ...stored(storage),
    pending: stored(storage + ".save"),
    applyCommand: stored(storage + ".apply"),
    saveAcknowledgement: stored(storage + ".saved"),
    applyAcknowledgement: stored(storage + ".applied"),
  };
}
function reportOf(draft) {
  return draft.pending?.draft.report ?? draft.report;
}

function viewIdentity(draft) {
  const report = reportOf(draft);
  const accepted = draft.saveAcknowledgement?.report === report ? draft.saveAcknowledgement?.command : null;
  return JSON.stringify([report, accepted]);
}

const journalChanged = storage + ".changed";
const acknowledgementKey = kind => storage + (kind === "save" ? ".saved" : ".applied");

async function claimCommand(kind, command) {
  return navigator.locks.request(storage + "." + kind, () => {
    const key = storage + "." + kind;
    const current = stored(key);
    if (current) return current.command === command.command;
    if (stored(acknowledgementKey(kind))?.command === command.command) return false;
    localStorage.setItem(key, JSON.stringify(command));
    window.dispatchEvent(new Event(journalChanged));
    return true;
  });
}

async function settleCommand(kind, command, acknowledgement) {
  return navigator.locks.request(storage + "." + kind, () => {
    const key = storage + "." + kind;
    if (stored(key)?.command !== command.command) return false;
    if (acknowledgement) {
      localStorage.setItem(acknowledgementKey(kind), JSON.stringify({
        command: command.command, ...acknowledgement,
      }));
    }
    localStorage.removeItem(key);
    window.dispatchEvent(new Event(journalChanged));
    return true;
  });
}
function notation(game) {
  return game.history({verbose:true}).map(m => (m.color === "w" ? m.before.split(" ")[5] + ". " : "") + m.san).join(" ");
}
function lead(report, gap, reply) {
  const route = report.scan.route;
  const board = position({fen:route.fen, moves:[...route.moves, ...gap.moves]});
  if (reply) {
    const positionFields = fen => fen.split(" ").slice(0, 4).join(" ");
    if (positionFields(board.fen()) !== positionFields(gap.fen)) {
      throw new Error("The recorded line no longer reaches this gap.");
    }
    play(board, reply);
  }
  const pgn = board.pgn();
  return {
    pgn,
    text:notation(board) || board.fen(),
    url:"https://lichess.org/analysis/pgn/" + encodeURIComponent(pgn.replaceAll("+", "")) + "?color=" + report.scan.color,
  };
}

function stagedFor(report, index, draft) {
  return draft.stagedReply?.report === report?.id && draft.stagedReply?.gap === index
    ? draft.stagedReply : null;
}

function stagedLead(report, gap, index, draft) {
  const staged = stagedFor(report, index, draft);
  if (!staged) return null;
  try {
    const line = lead(report, gap, staged.movement);
    return draft.pgn === line.pgn ? line : null;
  } catch {
    return null;
  }
}
const percent = n => (100*n).toLocaleString(undefined,{maximumFractionDigits:2}) + "%";
const range = (value, unknown) => percent(value) + (unknown > 0.000001 ? " – " + percent(Math.min(1,value+unknown)) : "");

const noReplacements = Object.freeze({});

function ConflictDetails({ conflict, disabled, correct }) {
  const { saved, proposed } = conflict;
  const turn = conflict.turn === "white" ? "White" : "Black";
  return <li className="gap-conflict">
    <h4>{turn} to move, move {conflict.number}{conflict.transposition && " · Same position by transposition"}</h4>
    <dl>
      <dt>{conflict.saved_route_recorded ? "Recorded repertoire route" : "In your repertoire"}</dt>
      <dd>{saved ? (saved.line ?? `${saved.reply} (${conflict.saved_route_limited ? "saved route search limit reached" : "saved route unavailable"})`) :
        conflict.unresolved ? "Saved replies are unresolved at this position." : "No saved reply."}</dd>
      {conflict.saved_route_recorded && <dd>{conflict.saved_route_limited ? "Current route search limit reached." : "Current saved route unavailable."}</dd>}
      <dt>In the pasted PGN</dt>
      {proposed.map(reply => <React.Fragment key={reply.movement}>
        <dd>{reply.line}</dd>
        {reply.alternate_line && <dd>{reply.alternate_line}</dd>}
      </React.Fragment>)}
    </dl>
    {saved ? <div className="gap-conflict-actions">
      <button className="gap-recommended" disabled={disabled} onClick={() => correct(conflict)}>
        Use repertoire reply · {saved.reply} <span className="gap-recommendation">Recommended</span>
      </button>
      {proposed.filter(reply => reply.movement !== saved.movement).map(reply =>
        <button key={reply.movement} disabled={disabled} onClick={() => correct(conflict, reply)}>
          Use new reply in both · {reply.reply}
        </button>)}
      {proposed.some(reply => reply.movement !== saved.movement) && <small>Using a new reply changes saved repertoire lines after confirmation.</small>}
    </div> : <p>Edit the pasted PGN to resolve the competing replies.</p>}
  </li>;
}

function ReplacementImpact({ impact }) {
  return <ul className="gap-conflict-list">{impact.map((line, index) => <li key={index}>
    <strong>{line.name}</strong>
    <dl>
      <dt>Before</dt><dd>{line.before}</dd>
      <dt>After</dt><dd>{line.after ?? "Archived without an active continuation."}</dd>
    </dl>
    {line.stopped_at && <p>{line.stopped_at}</p>}
  </li>)}</ul>;
}

export function Gaps() {
  const [draft, setDraft] = useState(restore);
  const scanOwner = useRef(null);
  const saveProgress = useRef(null);
  const mounted = useRef(false);
  useEffect(() => {
    mounted.current = true;
    const storageChanged = event => {
      if (event.key === null || event.key === storage || event.key.startsWith(storage + ".")) refreshJournal();
    };
    window.addEventListener(journalChanged, refreshJournal);
    window.addEventListener("storage", storageChanged);
    refreshJournal();
    return () => {
      mounted.current = false;
      scanOwner.current?.controller.abort();
      saveProgress.current?.stop();
      window.removeEventListener(journalChanged, refreshJournal);
      window.removeEventListener("storage", storageChanged);
    };
  }, []);
  const [loadedReport,setReport] = useState(null);
  const [selectedGap, setSelectedGap] = useState(null);
  const [summary, setSummary] = useState(null);
  const [summaryRetry, setSummaryRetry] = useState(0);
  const [preview,setPreview] = useState(null);
  const [trim,setTrim] = useState(null);
  const [confirmation,setConfirmation] = useState(null);
  const [work, setWork] = useState(null);
  const running = useRef(null);
  const [error,setError] = useState("");
  const [message,setMessage] = useState("");
  const [commandFeedback, setCommandFeedback] = useState(null);
  const [reload, setReload] = useState(0);
  const picker = useRepertoirePages(reload);
  const [addressed, setAddressed] = useState({});
  const [pickerLookupFailed, setPickerLookupFailed] = useState(false);
  const [pickerLookupRetry, setPickerLookupRetry] = useState(0);
  const [requestedReview, setRequestedReview] = useState(null);
  const gapHeading = useRef(null);
  function refreshJournal() {
    const current = restore();
    if (scanOwner.current && scanOwner.current.view !== viewIdentity(current)) {
      scanOwner.current.controller.abort();
    }
    if (saveProgress.current && !saveProgress.current.owns(current)) saveProgress.current.stop();
    setRequestedReview(request => request?.report === reportOf(current) &&
      request?.index === (current.pending?.draft.gap ?? current.index) ? request : null);
    setDraft(current);
  }
  function update(patch) {
    localStorage.setItem(storage, JSON.stringify({ ...stored(storage), ...patch }));
    refreshJournal();
  }
  const requestedReport = reportOf(draft);
  const report = loadedReport?.id === requestedReport ? loadedReport : null;
  const pendingSave = draft.pending?.draft.report === report?.id ? draft.pending : null;
  const gapIndex = pendingSave?.draft.gap ?? draft.index;
  const gap = report?.gaps[gapIndex] ??
    (selectedGap?.report === requestedReport && selectedGap?.index === gapIndex ? selectedGap?.gap : null);
  const headingKey = report && repertoireKey({ id: report.scan.repertoire, color: report.scan.color });
  const pagedSelection = picker.rows.find(book => repertoireKey(book) === draft.book);
  const pagedHeading = picker.rows.find(book => repertoireKey(book) === headingKey);
  const missingSelection = picker.loaded && !pagedSelection ? draft.book : null;
  const missingHeading = picker.loaded && !pagedHeading ? headingKey : null;
  useEffect(() => {
    setPickerLookupFailed(false);
    const keys = [...new Set([missingSelection, missingHeading].filter(Boolean))];
    setAddressed(prior => Object.fromEntries(keys.filter(key => prior[key]).map(key => [key, prior[key]])));
    if (!keys.length) return;
    return visibleRead(async signal => {
      let failed = false;
      for (const key of keys) {
        const separator = key.lastIndexOf("/");
        const id = key.slice(0, separator);
        const color = key.slice(separator + 1);
        try {
          if (!id || !["white", "black"].includes(color)) throw new Error("Invalid repertoire address");
          const row = await readView("openings/repertoires/" + encodeURIComponent(id) + "/" + color, signal);
          if (!row || repertoireKey(row) !== key) throw new Error("Repertoire unavailable");
          signal.throwIfAborted();
          setAddressed(prior => ({ ...prior, [key]: row }));
        } catch {
          if (signal.aborted) return;
          failed = true;
        }
      }
      if (!signal.aborted) setPickerLookupFailed(failed);
    });
  }, [missingSelection, missingHeading, reload, pickerLookupRetry]);
  const book = draft.book ? pagedSelection ?? addressed[draft.book] : picker.rows[0];
  const books = book && !pagedSelection && draft.book ? [book, ...picker.rows] : picker.rows;
  const reportBook = pagedHeading ?? addressed[headingKey];
  useEffect(() => {
    if (!requestedReport || (requestedReport === report?.id && !reload)) return;
    const ctrl = new AbortController();
    api("coverage/" + requestedReport + "/gaps", undefined, ctrl.signal).then(result => {
      if (!ctrl.signal.aborted && reportOf(restore()) === requestedReport) {
        setReport(result);
      }
    }).catch(error => { if (!ctrl.signal.aborted) setError(error.message); });
    return () => ctrl.abort();
  }, [reload, requestedReport]);
  const needsGapPage = Boolean(report && gapIndex >= report.gaps.length && gapIndex < report.total);
  useEffect(() => {
    if (!needsGapPage) return;
    const ctrl = new AbortController();
    const offset = Math.floor(gapIndex / 30) * 30;
    api("coverage/" + requestedReport + "/gaps?offset=" + offset, undefined, ctrl.signal)
      .then(page => {
        if (!ctrl.signal.aborted) setSelectedGap({ report: requestedReport, index: gapIndex, gap: page.gaps[gapIndex - offset] });
      })
      .catch(error => { if (!ctrl.signal.aborted) setError(error.message); });
    return () => ctrl.abort();
  }, [reload, requestedReport, gapIndex, needsGapPage]);
  const reportId = report?.id;
  useEffect(() => {
    if (!reportId) return;
    const controller = new AbortController();
    api("coverage/" + reportId + "/studied", undefined, controller.signal)
      .then(value => { if (!controller.signal.aborted && value.id === reportId) setSummary(value); })
      .catch(() => { if (!controller.signal.aborted) setSummary({ id: reportId, unavailable: true }); });
    return () => controller.abort();
  }, [reportId, reload, summaryRetry]);
  const currentSummary = summary?.id === reportId ? summary : null;
  async function run(fn, { command } = {}) {
    const task = command ? { command } : { view: viewIdentity(restore()) };
    running.current = task;
    setWork(task);
    setError("");
    setCommandFeedback(null);
    try {
      await fn();
    } catch (failure) {
      if (running.current !== task || (task.view && task.view !== viewIdentity(restore()))) return;
      if (command) setCommandFeedback({ command, error: failure.message });
      else setError(failure.message);
    } finally {
      if (running.current === task) {
        running.current = null;
        setWork(null);
      }
    }
  }
  async function scan(request, previous) {
    if (!mounted.current) return;
    scanOwner.current?.controller.abort();
    const controller = new AbortController();
    scanOwner.current = { controller, view: viewIdentity(restore()) };
    const signal = controller.signal;
    const scanDraft = localStorage.getItem(storage);
    const acceptedSave = localStorage.getItem(acknowledgementKey("save"));
    const acceptedApply = localStorage.getItem(acknowledgementKey("apply"));
    const {id}=await api("coverage",request,signal,{requireJson:true});
    const next=await api("coverage/"+id+"/gaps",undefined,signal);
    signal.throwIfAborted();
    if (localStorage.getItem(storage) !== scanDraft ||
        localStorage.getItem(acknowledgementKey("save")) !== acceptedSave ||
        localStorage.getItem(acknowledgementKey("apply")) !== acceptedApply ||
        stored(storage + ".save") || stored(storage + ".apply")) return;
    setReport(next);clearPreview();
    if (reportId === id) setSummaryRetry(n => n + 1);
    setRequestedReview(null);
    update({report:id,index:null,pgn:"",stagedReply:null,replacements:noReplacements,previous:null,savedReport:null});
    if(previous!=null)setMessage("Saved. Authored coverage moved from "+percent(previous)+" to "+percent(next.authored)+".");
  }
  function inputScan() {
    const game=new Chess();
    if(draft.line?.trim())game.loadPgn(draft.line);
    const history=game.history({verbose:true});
    return {repertoire:book.id,color:book.color,through:Number(draft.through??10),
      route:{fen:history[0]?.before??game.fen(),moves:history.map(m=>m.lan)}};
  }
  function open(index) {
    const latest = restore();
    if (latest.report !== report.id || latest.pending || latest.applyCommand ||
        latest.saveAcknowledgement?.report === report.id || latest.savedReport === report.id) return;
    if (latest.index !== index) {
      clearPreview();setMessage("");
      update({index,pgn:"",stagedReply:null,replacements:noReplacements});
    }
    setRequestedReview({ report: report.id, index });
  }

  function stageReply(movement) {
    const latest = restore();
    if (blocked || latest.report !== report.id || latest.index !== gapIndex ||
        latest.pending || latest.applyCommand || latest.saveAcknowledgement?.report === report.id ||
        latest.savedReport === report.id || Object.keys(latest.replacements ?? noReplacements).length ||
        (latest.pgn?.trim() && !stagedLead(report, gap, gapIndex, latest))) return;
    try {
      const { pgn } = lead(report, gap, movement);
      clearPreview();
      update({ pgn, stagedReply: { report: report.id, gap: gapIndex, movement } });
    } catch (failure) {
      setError(failure.message);
    }
  }
  function save(command, batched) {
    return run(() => sendSave(command, batched), { command: command.command });
  }
  async function sendSave(command, batched) {
    setMessage("");
    const showProgress = message => setCommandFeedback({ command: command.command, message });
    showProgress(batched ? "Saving continuation in batches…" : "Saving continuation…");
    if (!await claimCommand("save", command)) {
      refreshJournal();
      setReload(n => n + 1);
      return;
    }
    refreshJournal();
    const stopProgress = command.replacements ? undefined : observeSaveProgress(command, showProgress);
    let receipt;
    try {
      receipt=await api(command.replacements ? "coverage/replace" : "coverage/save",command,undefined,{requireJson:true});
    } catch(e) {
      const stale = e.code === "stale_gap" || e.code === "gap_save_interrupted";
      if(stale || e.code==="opening_reply_conflict" || e.code==="replacement_over_budget" || e.code==="replacement_preview_unavailable") {
        if (await settleCommand("save", command)) {
          clearPreview();
          if (stale) setReport(old => old?.id === command.draft.report ? {...old, stale:true} : old);
          setError(e.message);
        }
        return;
      }
      throw e;
    } finally {
      stopProgress?.();
    }
    if (receipt?.command !== command.command || !Array.isArray(receipt.lines) || !receipt.lines.length ||
        !receipt.lines.every(line => typeof line === "string" && line.length > 0))
      throw new Error("Save acknowledgement was incomplete. Retry the same save.");
    const previous = report.model_superseded ? null : report.authored;
    const accepted = await settleCommand("save", command, {
      report: command.draft.report, replacement: Boolean(command.replacements), previous,
    });
    if (accepted && mounted.current && stored(storage)?.report === command.draft.report) {
      clearPreview();
      await run(() => scan(report.scan, previous));
    }
  }
  function observeSaveProgress(command, showProgress) {
    const delivery = running.current;
    const view = viewIdentity(restore());
    let timer;
    let request;
    const observer = { owns, stop };

    function owns(current) {
      return mounted.current && running.current === delivery &&
        current.pending?.command === command.command && viewIdentity(current) === view;
    }
    function eligible() {
      return saveProgress.current === observer && owns(restore()) && !document.hidden;
    }
    function schedule() {
      clearTimeout(timer);
      if (eligible() && !request) timer = setTimeout(poll, 750);
    }
    async function poll() {
      if (!eligible() || request) return;
      const reading = new AbortController();
      request = reading;
      try {
        const state = await readView("coverage/save/" + command.command, reading.signal);
        if (state && eligible() && !reading.signal.aborted) {
          showProgress(`Saving continuation. ${state.finished} of ${state.total} lines complete…`);
        }
      } catch { /* The save command has its own acknowledgement. */ }
      finally {
        request = undefined;
        schedule();
      }
    }
    function visibilityChanged() {
      if (document.hidden) {
        clearTimeout(timer);
        request?.abort();
      } else {
        schedule();
      }
    }
    function stop() {
      if (saveProgress.current === observer) saveProgress.current = null;
      clearTimeout(timer);
      request?.abort();
      document.removeEventListener("visibilitychange", visibilityChanged);
    }

    if (!owns(restore())) return;
    saveProgress.current?.stop();
    saveProgress.current = observer;
    document.addEventListener("visibilitychange", visibilityChanged);
    schedule();
    return stop;
  }
  function applyOrder(pending) {
    return run(() => sendApply(pending), { command: pending.command });
  }
  async function sendApply(pending) {
    if (!await claimCommand("apply", pending)) {
      refreshJournal();
      return;
    }
    refreshJournal();
    try {
      const reply = await api("coverage/" + pending.report + "/apply", { command: pending.command });
      if (reply?.command !== pending.command || reply.id !== pending.report ||
          !Number.isSafeInteger(reply.cards) || reply.cards < 0) {
        throw new Error("Retry applying the opening order.");
      }
      await settleCommand("apply", pending, { report: reply.id, cards: reply.cards });
    } catch(e) {
      if (e.code === "stale_gap" || e.code === "coverage_model_superseded") {
        if (await settleCommand("apply", pending)) setError(e.message);
        return;
      }
      throw e;
    }
  }
  const isPending = command => command === draft.pending?.command || command === draft.applyCommand?.command;
  const busy = !!work && (work.command ? isPending(work.command) : work.view === viewIdentity(draft));
  const locked = busy || !!draft.pending || !!draft.applyCommand;
  const staged = gap && report ? stagedLead(report, gap, gapIndex, draft) : null;
  const edit={report:report?.id,gap:draft.index,pgn:draft.pgn??"",
    ...(stagedFor(report, gapIndex, draft) ? { anchor: "recorded_lead_in" } : {})};
  const replacements = draft.replacements ?? noReplacements;
  const hasReplacements = Object.keys(replacements).length > 0;

  function clearPreview() {
    setPreview(null);
    setTrim(null);
    setConfirmation(null);
  }
  function isCurrent(candidate, choices) {
    const latest = restore();
    const anchor = stagedFor(report, gapIndex, latest) ? "recorded_lead_in" : undefined;
    return candidate.anchor === anchor && mounted.current && latest.report === candidate.report && latest.index === candidate.gap &&
      latest.pgn === candidate.pgn && latest.saveAcknowledgement?.report !== candidate.report &&
      latest.savedReport !== candidate.report &&
      JSON.stringify(latest.replacements ?? noReplacements) === JSON.stringify(choices);
  }
  async function previewDraft(candidate, choices) {
    clearPreview();
    const view = await api("coverage/preview", { ...candidate, replacements: choices }, undefined, { requireJson: true });
    if (isCurrent(candidate, choices)) setPreview({ ...view, draft: candidate, choices });
  }
  async function applyCorrection(correction) {
    if (!isCurrent(correction.draft, correction.choices)) return;
    const next = { ...correction.draft, pgn: correction.pgn };
    update({ pgn: next.pgn, replacements: correction.nextChoices });
    await previewDraft(next, correction.nextChoices);
  }
  async function correct(conflict, reply) {
    setTrim(null);
    setConfirmation(null);
    const correction = await api("coverage/correct", {
      draft: edit, position: conflict.position, movement: reply?.movement,
    }, undefined, { requireJson: true });
    if (!isCurrent(edit, replacements)) return;
    const nextChoices = reply ? { ...replacements, [conflict.position]: reply.movement } : replacements;
    const outcome = { ...correction, draft: edit, choices: replacements, nextChoices };
    if (outcome.trims.length) setTrim(outcome);
    else await applyCorrection(outcome);
  }
  async function removeChoice(position) {
    const next = { ...replacements };
    delete next[position];
    update({ replacements: next });
    await previewDraft(edit, next);
  }
  function showMoreGaps() {
    const { id, gaps } = report;
    const offset = gaps.length;
    const view = viewIdentity(restore());
    return run(async () => {
      const page = await api("coverage/" + id + "/gaps?offset=" + offset);
      if (!mounted.current || viewIdentity(restore()) !== view || page.id !== id) return;
      setReport(current => current?.id === id && current.gaps.length === offset
        ? { ...page, gaps: [...current.gaps, ...page.gaps] }
        : current);
    });
  }
  const currentPreview = preview && isCurrent(preview.draft, preview.choices) ? preview : null;
  const conflicts = currentPreview?.conflicts ?? [];
  const replacement = currentPreview?.replacement;
  const confirming = replacement && confirmation === replacement.command;
  const saveAcknowledgement = draft.saveAcknowledgement?.report === reportId ? draft.saveAcknowledgement : null;
  const applyAcknowledgement = draft.applyAcknowledgement?.report === reportId ? draft.applyAcknowledgement : null;
  const saved = saveAcknowledgement || (draft.savedReport && draft.savedReport === reportId);
  const previous = saveAcknowledgement ? saveAcknowledgement.previous : draft.previous;
  const savedMessage = saveAcknowledgement?.replacement ? "Repertoire changes and continuation saved." : "Continuation saved.";
  const appliedMessage = applyAcknowledgement && "Applied opening order for "+applyAcknowledgement.cards+" reached cards. Due opening reviews use this order.";
  const pendingFeedback = commandFeedback && isPending(commandFeedback.command) ? commandFeedback : null;
  const alert = error || pendingFeedback?.error;
  const status = pendingFeedback?.message || (!draft.pending && saved && savedMessage) || message;
  const blocked = locked || report?.stale || Boolean(saved);
  const blockedSelection = locked || Boolean(saved);
  const reviewStart = requestedReview?.report === reportId && requestedReview?.index === gapIndex ? requestedReview : null;
  const hasGap = Boolean(gap);
  useEffect(() => {
    if (!reviewStart || !hasGap || !gapHeading.current) return;
    gapHeading.current.focus({ preventScroll: true });
    gapHeading.current.scrollIntoView({ block: "start" });
  }, [reviewStart, hasGap]);
  return <section><h2>Repertoire gaps</h2>
    <fieldset disabled={locked}><label>Repertoire <select value={book ? repertoireKey(book) : ""} onChange={e=>update({book:e.target.value})}>
      {!book && <option value="" disabled>Select a repertoire</option>}
      {books.map(b=><option key={repertoireKey(b)} value={repertoireKey(b)}>{b.example_line} ({b.color})</option>)}</select></label>
      {picker.error && <p role="alert">{picker.error.message}</p>}
      {(picker.error || picker.more) && <button type="button" disabled={picker.busy}
        onClick={() => picker.load(picker.error?.append ?? true)}>
        {picker.error ? "Retry loading repertoires" : "Load more repertoires"}
      </button>}
      {pickerLookupFailed && <button type="button" onClick={() => setPickerLookupRetry(count => count + 1)}>
        Retry saved repertoire lookup
      </button>}
      <label>Starting line (PGN) <input placeholder="1. e4 e5 2. Nf3 Nc6 3. Bc4" value={draft.line??""} onChange={e=>update({line:e.target.value})}/></label>
      <label>Through my move <input type="number" min="1" max="100" value={draft.through??10} onChange={e=>update({through:e.target.value})}/></label>
      <button disabled={!book} onClick={()=>run(()=>scan(inputScan()))}>Scan repertoire</button></fieldset>
    {busy && <p role="status">Working…</p>}{alert && <p role="alert">{alert}</p>}
    {error && (!report || !books.length) && <button disabled={busy} onClick={() => setReload(n => n + 1)}>Reload gaps</button>}{status && <p role="status">{status}</p>}
    {!draft.applyCommand && appliedMessage && <p role="status">{appliedMessage}</p>}
    {draft.applyCommand && <button disabled={busy} onClick={() => applyOrder(draft.applyCommand)}>Retry applying order</button>}
    {pendingSave && !gap && <button disabled={busy} onClick={() => save(pendingSave)}>Retry same save</button>}
    {draft.pending && !pendingSave && <button disabled={busy} onClick={() => setReload(n => n + 1)}>Load pending save</button>}
    {report && <><h3>{reportBook?.example_line ?? "Repertoire"} · {report.scan.color}</h3>
      <p>From {notation(position(report.scan.route))||"the starting position"} through your move {report.scan.through} · {report.model}</p>
      <p>Authored {range(report.authored,report.unresolved)} · Personally studied {currentSummary?.unavailable ? <button onClick={() => setSummaryRetry(n => n + 1)}>Retry summary</button> : currentSummary ? range(currentSummary.studied,report.unresolved) : "Loading…"}</p>
      <p>Ended early {percent(report.ended)} · Unresolved {percent(report.unresolved)} (conflicting replies {percent(report.conflict)})</p>
      {report.model_superseded && <p>Rescan with the current model before applying this order to due opening reviews.</p>}
      {(report.model_superseded||report.stale||error||saved||previous!=null) && <button disabled={locked} onClick={()=>run(()=>scan(report.scan,report.model_superseded ? null : previous))}>Retry scan</button>}
      <button disabled={blocked||report.model_superseded} onClick={()=>applyOrder({ report: report.id, command: crypto.randomUUID() })}>Apply to opening order for due reviews</button>
      {report.gaps.length>0 && <button disabled={blockedSelection} onClick={()=>open(0)}>Work on next gap</button>}
      {gap && <section><h3 ref={gapHeading} tabIndex={-1}>Your {report.scan.color} reply</h3><p>{lead(report,gap).text}</p>
        {!saved && <GapReview
          key={JSON.stringify([viewIdentity(draft), gapIndex])}
          report={report.id} index={gapIndex} fen={gap.fen} color={report.scan.color}
          start={reviewStart} staged={staged ? draft.stagedReply.movement : null} canReview={!locked}
          canStage={!blocked && !hasReplacements && (!draft.pgn?.trim() || !!staged)}
          canRescan={!locked} onStage={stageReply}
          onRescan={() => run(() => scan(report.scan))} />}
        <a href={(staged ?? lead(report,gap)).url} target="_blank" rel="noreferrer">
          {staged ? "Extend staged reply in Lichess" : "Open line in Lichess"}
        </a>
        <label>Paste PGN from Lichess <textarea disabled={locked} value={draft.pgn??""} onChange={e=>{clearPreview();update({pgn:e.target.value});}}/></label>
        {stagedFor(report, gapIndex, draft) && !staged && <button type="button" disabled={locked}
          onClick={() => { clearPreview(); update({stagedReply:null,replacements:noReplacements}); }}>
          Use pasted line’s own lead-in
        </button>}
        <div className="controls">
          <button disabled={blocked || !edit.pgn}
            onClick={() => run(() => previewDraft(edit, replacements))}>Preview</button>
          {pendingSave
            ? <button disabled={busy} onClick={() => save(pendingSave)}>Retry same save</button>
            : hasReplacements
              ? <button disabled={blocked || !!trim || !replacement || replacement.over_budget || confirming}
                  onClick={() => setConfirmation(replacement.command)}>Review repertoire changes</button>
              : <button disabled={blocked || !!trim || !currentPreview || conflicts.length > 0 ||
                    currentPreview.responses.some(reply => reply.status === "conflict")}
                  onClick={() => save({ draft: currentPreview.draft, command: currentPreview.command }, currentPreview.batched)}>Save to repertoire</button>}
        </div>
        {currentPreview && <p role="status">{currentPreview.new_replies} new {currentPreview.new_replies === 1 ? "reply" : "replies"} · {conflicts.length ? `${conflicts.length} ${conflicts.length === 1 ? "conflict" : "conflicts"}` : "No remaining conflicts"}</p>}
        {!!conflicts.length && <ol className="gap-conflict-list">{conflicts.map(conflict =>
          <ConflictDetails key={conflict.position} conflict={conflict} disabled={blocked || !!trim}
            correct={(item, reply) => run(() => correct(item, reply))} />)}</ol>}
        {trim && <section className="gap-confirmation" aria-label="Review PGN trims">
          <h4>These branches would lose later moves</h4>
          <ul>{trim.trims.map((item, i) => <li key={i}>Stops before {item.first_removed} in {item.line}</li>)}</ul>
          <div className="controls">
            <button disabled={blocked} onClick={() => run(() => applyCorrection(trim))}>Keep legal moves</button>
            <button disabled={locked} onClick={() => setTrim(null)}>Keep editing</button>
          </div>
        </section>}
        {currentPreview?.staged?.length > 0 && <section aria-label="Staged repertoire changes">
          <h4>Staged repertoire changes</h4>
          <ul>{currentPreview.staged.map(choice => <li key={choice.position}>
            {choice.valid ? `${choice.old} → ${choice.new}` : `${choice.new} no longer matches this draft`}{" "}
            <button disabled={blocked || !!trim} onClick={() => run(() => removeChoice(choice.position))}>Remove change</button>
          </li>)}</ul>
        </section>}
        {!currentPreview && hasReplacements && <div className="controls">
          <button disabled={locked} onClick={() => { clearPreview(); update({ replacements: noReplacements }); }}>Clear staged repertoire changes</button>
        </div>}
        {replacement && <section className={confirming ? "gap-confirmation" : undefined} aria-label="Saved line changes">
          <h4>{confirming ? "Confirm changes to your saved repertoire" : "Affected saved lines"}</h4>
          {replacement.impact.length ? <ReplacementImpact impact={replacement.impact} /> : <p>The saved reply will change at the selected position.</p>}
          {replacement.over_budget && <p>This change is too large to save at once. Split the PGN and preview again.</p>}
          {confirming && <div className="controls">
            <button disabled={blocked} onClick={() => setConfirmation(null)}>Cancel</button>
            <button disabled={blocked} onClick={() => save({
              draft: currentPreview.draft, replacements, command: replacement.command, revision: replacement.revision,
              line_revision: replacement.line_revision,
            })}>Confirm repertoire changes and save</button>
          </div>}
        </section>}
      </section>}
      <ol>{report.gaps.map((g,i)=><li key={g.fen}><button disabled={blockedSelection} onClick={()=>open(i)}>{percent(g.probability)} · {lead(report,g).text}</button></li>)}</ol>
      {report.gaps.length < report.total && <button disabled={busy} onClick={showMoreGaps}>Show more gaps</button>}
    </>}
  </section>;
}
