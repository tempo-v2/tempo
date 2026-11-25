import React, { useEffect, useRef, useState } from "react";
import { Chess } from "chess.js";
import { api } from "./api.js";
import { position } from "./board.jsx";

const storage = "tempo-lean-gap-draft-v1";
function stored(key) {
  try { return JSON.parse(localStorage.getItem(key)); } catch { return null; }
}
function restore() {
  return { ...stored(storage), pending: stored(storage + ".save"), applyCommand: stored(storage + ".apply") };
}
async function claimCommand(kind, command) {
  return navigator.locks.request(storage + "." + kind, () => {
    const key = storage + "." + kind;
    const current = stored(key);
    if (current) return current.command === command.command;
    localStorage.setItem(key, JSON.stringify(command));
    return true;
  });
}
async function clearCommand(kind, command) {
  await navigator.locks.request(storage + "." + kind, () => {
    const key = storage + "." + kind;
    if (stored(key)?.command === command.command) localStorage.removeItem(key);
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

export function Gaps() {
  const [draft, setDraft] = useState(restore);
  const form = useRef(stored(storage));
  const [books, setBooks] = useState([]);
  const [report,setReport] = useState(null);
  const [preview,setPreview] = useState(null);
  const [busy,setBusy] = useState(false);
  const [error,setError] = useState("");
  const [message,setMessage] = useState("");
  const [reload, setReload] = useState(0);
  function refreshJournal() {
    setDraft({ ...form.current, pending: stored(storage + ".save"), applyCommand: stored(storage + ".apply") });
  }
  function update(patch) {
    form.current = { ...form.current, ...patch };
    localStorage.setItem(storage, JSON.stringify(form.current));
    refreshJournal();
  }
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
    async function loadReport() {
      const saved = { ...form.current, pending: stored(storage + ".save") };
      const reportId = saved.pending?.draft.report ?? saved.report;
      if (!reportId) return;
      const result = await api("coverage/" + reportId, undefined, ctrl.signal);
      // Recover an editor opened on a later page without fetching intervening pages.
      const index = saved.pending?.draft.gap ?? saved.index;
      if (index >= result.gaps.length && index < result.total) {
        const offset = Math.floor(index / 30) * 30;
        const page = await api("coverage/" + reportId + "?offset=" + offset, undefined, ctrl.signal);
        result.selectedGap = page.gaps[index - offset];
      }
      const latest = { ...form.current, pending: stored(storage + ".save") };
      if ((latest.pending?.draft.report ?? latest.report) === reportId) setReport(result);
    }
    setError("");
    const failed = e => { if (!ctrl.signal.aborted) setError(e.message); };
    void loadBooks().catch(failed);
    void loadReport().catch(failed);
    return () => ctrl.abort();
  }, [reload]);
  async function run(fn) {
    setBusy(true);setError("");
    try { await fn(); } catch(e){setError(e.message);} finally {setBusy(false);}
  }
  const book = draft.book ? books.find(b => b.id + "/" + b.color === draft.book) : books[0];
  async function scan(request, previous) {
    const {id}=await api("coverage",request);
    const next=await api("coverage/"+id);
    setReport(next);setPreview(null);
    update({report:id,index:null,pgn:"",previous:null});
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
    setPreview(null);setMessage("");update({index,pgn:""});
  }
  async function save(command) {
    if (!await claimCommand("save", command)) {
      refreshJournal();
      setReload(n => n + 1);
      return;
    }
    refreshJournal();
    let receipt;
    try {
      receipt=await api("coverage/save",command,undefined,{requireJson:true});
    } catch(e) {
      if(e.code==="stale_gap") {
        await clearCommand("save", command);
        refreshJournal();
        setPreview(null);
        setReport(old=>({...old,stale:true}));
      }
      throw e;
    }
    if(receipt?.command!==command.command || !Array.isArray(receipt.lines) || !receipt.lines.length)
      throw new Error("Save acknowledgement was incomplete. Retry the same save.");
    setReport(old=>({...old,stale:true}));
    setMessage("Continuation saved. Refreshing coverage…");
    await clearCommand("save", command);
    update({previous:report.authored});
    setPreview(null);
    await scan(report.scan,report.authored);
  }
  async function applyOrder() {
    const pending = restore().applyCommand ?? { report: report.id, command: crypto.randomUUID() };
    if (!await claimCommand("apply", pending)) {
      refreshJournal();
      return;
    }
    refreshJournal();
    try {
      const reply = await api("coverage/" + pending.report + "/apply", { command: pending.command });
      if(reply?.command!==pending.command || reply.id!==pending.report)throw new Error("Retry applying the opening order.");
      await clearCommand("apply", pending);
      refreshJournal();
      setMessage("Applied opening order for "+reply.cards+" reached cards. New cards use this order when Maia is selected in Settings.");
    } catch(e) {
      if (e.code === "stale_gap") {
        await clearCommand("apply", pending);
        refreshJournal();
      }
      throw e;
    }
  }
  const locked=busy || !!draft.pending || !!draft.applyCommand;
  const edit={report:report?.id,gap:draft.index,pgn:draft.pgn??""};
  const currentPreview = preview && preview.draft.report === edit.report &&
    preview.draft.gap === edit.gap && preview.draft.pgn === edit.pgn ? preview : null;
  return <section><h2>Repertoire gaps</h2>
    <fieldset disabled={locked}><label>Repertoire <select value={book ? book.id+"/"+book.color : ""} onChange={e=>update({book:e.target.value})}>
      {!book && <option value="" disabled>Select a repertoire</option>}
      {books.map(b=><option key={b.id+b.color} value={b.id+"/"+b.color}>{b.example_line} ({b.color})</option>)}</select></label>
      <label>Starting line (PGN) <input placeholder="1. e4 e5 2. Nf3 Nc6 3. Bc4" value={draft.line??""} onChange={e=>update({line:e.target.value})}/></label>
      <label>Through my move <input type="number" min="1" max="100" value={draft.through??10} onChange={e=>update({through:e.target.value})}/></label>
      <button disabled={!book} onClick={()=>run(()=>scan(inputScan()))}>Scan repertoire</button></fieldset>
    {busy && <p role="status">Working…</p>}{error && <p role="alert">{error}</p>}
    {error && (!report || !books.length) && <button disabled={busy} onClick={() => setReload(n => n + 1)}>Reload gaps</button>}{message && <p role="status">{message}</p>}
    {draft.applyCommand && <button disabled={busy} onClick={() => run(applyOrder)}>Retry applying order</button>}
    {draft.pending && !pendingSave && <button disabled={busy} onClick={() => setReload(n => n + 1)}>Load pending save</button>}
    {report && <><h3>{books.find(b=>b.id===report.scan.repertoire&&b.color===report.scan.color)?.example_line} · {report.scan.color}</h3>
      <p>From {notation(position(report.scan.route))||"the starting position"} through your move {report.scan.through} · Maia-2 Rapid 1500/1500</p>
      <p>Authored {range(report.authored,report.unresolved)} · Personally studied {range(report.studied,report.unresolved)}</p>
      <p>Ended early {percent(report.ended)} · Unresolved {percent(report.unresolved)} (conflicting replies {percent(report.conflict)})</p>
      {(report.stale||error||draft.previous!=null) && <button disabled={locked} onClick={()=>run(()=>scan(report.scan,draft.previous))}>Retry scan</button>}
      <button disabled={locked||report.stale} onClick={()=>run(applyOrder)}>Apply to opening order</button>
      {report.gaps.length>0 && <button disabled={locked||report.stale} onClick={()=>open(0)}>Work on next gap</button>}
      {gap && <section><h3>Your {report.scan.color} reply</h3><p>{lead(report,gap).text}</p>
        <a href={lead(report,gap).url} target="_blank" rel="noreferrer">Open line in Lichess</a>
        <label>Paste PGN from Lichess <textarea disabled={locked} value={draft.pgn??""} onChange={e=>{setPreview(null);update({pgn:e.target.value});}}/></label>
        <button disabled={locked||report.stale||!edit.pgn} onClick={()=>run(async()=>setPreview({ ...await api("coverage/preview",edit), draft: edit }))}>Preview</button>
        {currentPreview && <><ul>{currentPreview.responses.map(r=><li key={r.position}>{r.move} · {r.status}</li>)}</ul>
          <button disabled={locked||currentPreview.responses.some(r=>r.status==="conflict")} onClick={()=>run(()=>save({draft:currentPreview.draft,command:currentPreview.command}))}>Save to repertoire</button></>}
        {pendingSave && <button disabled={busy} onClick={()=>run(()=>save(pendingSave))}>Retry same save</button>}
      </section>}
      <ol>{report.gaps.map((g,i)=><li key={g.fen}><button disabled={locked||report.stale} onClick={()=>open(i)}>{percent(g.probability)} · {lead(report,g).text}</button></li>)}</ol>
      {report.gaps.length<report.total && <button disabled={busy} onClick={()=>run(async()=>{
        const page=await api("coverage/"+report.id+"?offset="+report.gaps.length);
        setReport({...page,selectedGap:report.selectedGap,gaps:[...report.gaps,...page.gaps]});
      })}>Show more gaps</button>}
    </>}
  </section>;
}
