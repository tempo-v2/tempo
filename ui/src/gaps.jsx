import React, { useEffect, useRef, useState } from "react";
import { Chess } from "chess.js";
import { api } from "./api.js";
import { position } from "./board.jsx";

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
function lead(report, gap) {
  const route = report.scan.route;
  const board = position({fen:route.fen, moves:[...route.moves, ...gap.moves]});
  const pgn = board.pgn().replaceAll("+", "");
  return { text:notation(board) || board.fen(), url:"https://lichess.org/analysis/pgn/" + encodeURIComponent(pgn) + "?color=" + report.scan.color };
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
  const form = useRef(stored(storage));
  const scanOwner = useRef(null);
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
      scanOwner.current?.abort();
      window.removeEventListener(journalChanged, refreshJournal);
      window.removeEventListener("storage", storageChanged);
    };
  }, []);
  const [books, setBooks] = useState([]);
  const [report,setReport] = useState(null);
  const [summary, setSummary] = useState(null);
  const [summaryRetry, setSummaryRetry] = useState(0);
  const [preview,setPreview] = useState(null);
  const [trim,setTrim] = useState(null);
  const [confirmation,setConfirmation] = useState(null);
  const [busy,setBusy] = useState(false);
  const [error,setError] = useState("");
  const [message,setMessage] = useState("");
  const [reload, setReload] = useState(0);
  function refreshJournal() {
    const current = restore();
    form.current = current;
    setDraft(current);
  }
  function update(patch) {
    localStorage.setItem(storage, JSON.stringify({ ...stored(storage), ...patch }));
    refreshJournal();
  }
  const requestedReport = draft.pending?.draft.report ?? draft.report;
  useEffect(() => {
    const ctrl=new AbortController();
    async function loadBooks() {
      let options = [], page;
      do {
        const tail = options.at(-1);
        const query = tail ? "?" + new URLSearchParams({ after: tail.id, color: tail.color }) : "";
        page = await api("openings/repertoires" + query, undefined, ctrl.signal);
        options = options.concat(page);
      } while (page.length === 50);
      setBooks(options);
    }
    setError("");
    void loadBooks().catch(error => { if (!ctrl.signal.aborted) setError(error.message); });
    return () => ctrl.abort();
  }, [reload]);
  useEffect(() => {
    const ctrl = new AbortController();
    async function loadReport() {
      const saved = { ...form.current, pending: stored(storage + ".save") };
      const reportId = saved.pending?.draft.report ?? saved.report;
      if (!reportId) return;
      const result = await api("coverage/" + reportId + "/gaps", undefined, ctrl.signal);
      // Recover an editor opened on a later page without fetching intervening pages.
      const index = saved.pending?.draft.gap ?? saved.index;
      if (index >= result.gaps.length && index < result.total) {
        const offset = Math.floor(index / 30) * 30;
        const page = await api("coverage/" + reportId + "/gaps?offset=" + offset, undefined, ctrl.signal);
        result.selectedGap = page.gaps[index - offset];
      }
      const latest = restore();
      if (!ctrl.signal.aborted && (latest.pending?.draft.report ?? latest.report) === reportId) setReport(result);
    }
    if (requestedReport !== report?.id || reload) {
      void loadReport().catch(error => { if (!ctrl.signal.aborted) setError(error.message); });
    }
    return () => ctrl.abort();
  }, [reload, requestedReport]);
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
  async function run(fn) {
    setBusy(true);setError("");
    try { await fn(); } catch(e){setError(e.message);} finally {setBusy(false);}
  }
  const book = draft.book ? books.find(b => b.id + "/" + b.color === draft.book) : books[0];
  async function scan(request, previous) {
    if (!mounted.current) return;
    scanOwner.current?.abort();
    const controller = new AbortController();
    scanOwner.current = controller;
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
    update({report:id,index:null,pgn:"",replacements:noReplacements,previous:null,savedReport:null});
    if(previous!=null)setMessage("Saved. Authored coverage moved from "+percent(previous)+" to "+percent(next.authored)+".");
  }
  function inputScan() {
    const game=new Chess();
    if(draft.line?.trim())game.loadPgn(draft.line);
    const history=game.history({verbose:true});
    return {repertoire:book.id,color:book.color,through:Number(draft.through??10),
      route:{fen:history[0]?.before??game.fen(),moves:history.map(m=>m.lan)}};
  }
  const pendingSave = draft.pending?.draft.report === report?.id ? draft.pending : null;
  const gap=report?.gaps[pendingSave?.draft.gap ?? draft.index]??report?.selectedGap;
  function open(index) {
    const link=lead(report,report.gaps[index]);
    window.open(link.url,"_blank","noopener,noreferrer");
    clearPreview();setMessage("");update({index,pgn:"",replacements:noReplacements});
  }
  async function save(command, batched) {
    setMessage(batched ? "Saving continuation in batches…" : "Saving continuation…");
    if (!await claimCommand("save", command)) {
      refreshJournal();
      setReload(n => n + 1);
      return;
    }
    refreshJournal();
    const progress = new AbortController();
    const timer = command.replacements ? null : setInterval(async () => {
      try {
        const state = await api("coverage/save/" + command.command, undefined, progress.signal);
        if (state && !progress.signal.aborted) setMessage(`Saving continuation. ${state.finished} of ${state.total} lines complete…`);
      } catch { /* The save command has its own acknowledgement. */ }
    }, 750);
    let receipt;
    try {
      receipt=await api(command.replacements ? "coverage/replace" : "coverage/save",command,undefined,{requireJson:true});
    } catch(e) {
      const stale = e.code === "stale_gap" || e.code === "gap_save_interrupted";
      if(stale || e.code==="opening_reply_conflict" || e.code==="replacement_over_budget" || e.code==="replacement_preview_unavailable") {
        await settleCommand("save", command);
        refreshJournal();
        clearPreview();
        if (stale) setReport(old=>({...old,stale:true}));
      }
      setMessage("");
      throw e;
    } finally {
      clearInterval(timer);
      progress.abort();
    }
    if(receipt?.command!==command.command || !Array.isArray(receipt.lines) || !receipt.lines.length)
      throw new Error("Save acknowledgement was incomplete. Retry the same save.");
    const previous = report.model_superseded ? null : report.authored;
    const accepted = await settleCommand("save", command, {
      report: command.draft.report, replacement: Boolean(command.replacements), previous,
    });
    setMessage("");
    clearPreview();
    if (accepted && stored(storage)?.report === command.draft.report) await scan(report.scan, previous);
  }
  async function applyOrder(pending) {
    if (!await claimCommand("apply", pending)) {
      refreshJournal();
      return;
    }
    refreshJournal();
    try {
      const reply = await api("coverage/" + pending.report + "/apply", { command: pending.command });
      if(reply?.command!==pending.command || reply.id!==pending.report)throw new Error("Retry applying the opening order.");
      await settleCommand("apply", pending, { report: reply.id, cards: reply.cards });
    } catch(e) {
      if (e.code === "stale_gap" || e.code === "coverage_model_superseded") {
        await settleCommand("apply", pending);
        refreshJournal();
      }
      throw e;
    }
  }
  const locked=busy || !!draft.pending || !!draft.applyCommand;
  const edit={report:report?.id,gap:draft.index,pgn:draft.pgn??""};
  const replacements = draft.replacements ?? noReplacements;
  const hasReplacements = Object.keys(replacements).length > 0;

  function clearPreview() {
    setPreview(null);
    setTrim(null);
    setConfirmation(null);
  }
  function isCurrent(candidate, choices) {
    return mounted.current && form.current?.report === candidate.report && form.current?.index === candidate.gap &&
      form.current?.pgn === candidate.pgn &&
      JSON.stringify(form.current?.replacements ?? noReplacements) === JSON.stringify(choices);
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
  const currentPreview = preview && isCurrent(preview.draft, preview.choices) ? preview : null;
  const conflicts = currentPreview?.conflicts ?? [];
  const replacement = currentPreview?.replacement;
  const confirming = replacement && confirmation === replacement.command;
  const saveAcknowledgement = draft.saveAcknowledgement?.report === reportId ? draft.saveAcknowledgement : null;
  const applyAcknowledgement = draft.applyAcknowledgement?.report === reportId ? draft.applyAcknowledgement : null;
  const saved = saveAcknowledgement || (draft.savedReport && draft.savedReport === reportId);
  const previous = saveAcknowledgement ? saveAcknowledgement.previous : draft.previous;
  const savedMessage = saveAcknowledgement?.replacement ? "Repertoire changes and continuation saved." : "Continuation saved.";
  const appliedMessage = applyAcknowledgement && "Applied opening order for "+applyAcknowledgement.cards+" reached cards. New cards use this order when Maia is selected in Settings.";
  const status = message || (!draft.pending && saved && savedMessage);
  const blocked = locked || report?.stale || Boolean(saved);
  return <section><h2>Repertoire gaps</h2>
    <fieldset disabled={locked}><label>Repertoire <select value={book ? book.id+"/"+book.color : ""} onChange={e=>update({book:e.target.value})}>
      {!book && <option value="" disabled>Select a repertoire</option>}
      {books.map(b=><option key={b.id+b.color} value={b.id+"/"+b.color}>{b.example_line} ({b.color})</option>)}</select></label>
      <label>Starting line (PGN) <input placeholder="1. e4 e5 2. Nf3 Nc6 3. Bc4" value={draft.line??""} onChange={e=>update({line:e.target.value})}/></label>
      <label>Through my move <input type="number" min="1" max="100" value={draft.through??10} onChange={e=>update({through:e.target.value})}/></label>
      <button disabled={!book} onClick={()=>run(()=>scan(inputScan()))}>Scan repertoire</button></fieldset>
    {busy && <p role="status">Working…</p>}{error && <p role="alert">{error}</p>}
    {error && (!report || !books.length) && <button disabled={busy} onClick={() => setReload(n => n + 1)}>Reload gaps</button>}{status && <p role="status">{status}</p>}
    {!draft.applyCommand && appliedMessage && <p role="status">{appliedMessage}</p>}
    {draft.applyCommand && <button disabled={busy} onClick={() => run(() => applyOrder(draft.applyCommand))}>Retry applying order</button>}
    {draft.pending && !pendingSave && <button disabled={busy} onClick={() => setReload(n => n + 1)}>Load pending save</button>}
    {report && <><h3>{books.find(b=>b.id===report.scan.repertoire&&b.color===report.scan.color)?.example_line} · {report.scan.color}</h3>
      <p>From {notation(position(report.scan.route))||"the starting position"} through your move {report.scan.through} · {report.model}</p>
      <p>Authored {range(report.authored,report.unresolved)} · Personally studied {currentSummary?.unavailable ? <button onClick={() => setSummaryRetry(n => n + 1)}>Retry summary</button> : currentSummary ? range(currentSummary.studied,report.unresolved) : "Loading…"}</p>
      <p>Ended early {percent(report.ended)} · Unresolved {percent(report.unresolved)} (conflicting replies {percent(report.conflict)})</p>
      {report.model_superseded && <p>Rescan with the current model before using these probabilities to set your opening order.</p>}
      {(report.model_superseded||report.stale||error||saved||previous!=null) && <button disabled={locked} onClick={()=>run(()=>scan(report.scan,report.model_superseded ? null : previous))}>Retry scan</button>}
      <button disabled={blocked||report.model_superseded} onClick={()=>run(() => applyOrder({ report: report.id, command: crypto.randomUUID() }))}>Apply to opening order</button>
      {report.gaps.length>0 && <button disabled={blocked} onClick={()=>open(0)}>Work on next gap</button>}
      {gap && <section><h3>Your {report.scan.color} reply</h3><p>{lead(report,gap).text}</p>
        <a href={lead(report,gap).url} target="_blank" rel="noreferrer">Open line in Lichess</a>
        <label>Paste PGN from Lichess <textarea disabled={locked} value={draft.pgn??""} onChange={e=>{clearPreview();update({pgn:e.target.value});}}/></label>
        <div className="controls">
          <button disabled={blocked || !edit.pgn}
            onClick={() => run(() => previewDraft(edit, replacements))}>Preview</button>
          {pendingSave
            ? <button disabled={busy} onClick={() => run(() => save(pendingSave))}>Retry same save</button>
            : hasReplacements
              ? <button disabled={blocked || !!trim || !replacement || replacement.over_budget || confirming}
                  onClick={() => setConfirmation(replacement.command)}>Review repertoire changes</button>
              : <button disabled={blocked || !!trim || !currentPreview || conflicts.length > 0 ||
                    currentPreview.responses.some(reply => reply.status === "conflict")}
                  onClick={() => run(() => save({ draft: currentPreview.draft, command: currentPreview.command }, currentPreview.batched))}>Save to repertoire</button>}
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
            <button disabled={blocked} onClick={() => run(() => save({
              draft: currentPreview.draft, replacements, command: replacement.command, revision: replacement.revision,
              line_revision: replacement.line_revision,
            }))}>Confirm repertoire changes and save</button>
          </div>}
        </section>}
      </section>}
      <ol>{report.gaps.map((g,i)=><li key={g.fen}><button disabled={blocked} onClick={()=>open(i)}>{percent(g.probability)} · {lead(report,g).text}</button></li>)}</ol>
      {report.gaps.length<report.total && <button disabled={busy} onClick={()=>run(async()=>{
        const page=await api("coverage/"+report.id+"/gaps?offset="+report.gaps.length);
        setReport({...page,selectedGap:report.selectedGap,gaps:[...report.gaps,...page.gaps]});
      })}>Show more gaps</button>}
    </>}
  </section>;
}
