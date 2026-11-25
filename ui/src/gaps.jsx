import React, { useEffect, useState } from "react";
import { Chess } from "chess.js";
import { api } from "./api.js";
import { position } from "./board.jsx";

const storage = "tempo-lean-gap-draft-v1";
function restore() {
  try { return JSON.parse(localStorage.getItem(storage)) ?? {}; } catch { return {}; }
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
  const [books, setBooks] = useState([]);
  const [report,setReport] = useState(null);
  const [preview,setPreview] = useState(null);
  const [busy,setBusy] = useState(false);
  const [error,setError] = useState("");
  const [message,setMessage] = useState("");
  function update(patch) {
    setDraft(old => {
      const next={...old,...patch};
      localStorage.setItem(storage, JSON.stringify(next));
      return next;
    });
  }
  useEffect(() => {
    const ctrl=new AbortController();
    async function load() {
      let options=[], page;
      do {
        const tail=options.at(-1);
        page=await api("openings/repertoires"+(tail?"?"+new URLSearchParams({after:tail.id,color:tail.color}):""),undefined,ctrl.signal);
        options=options.concat(page);
      } while(page.length===50);
      setBooks(options);
      const saved=restore();
      if(saved.report) setReport(await api("coverage/"+saved.report,undefined,ctrl.signal));
    }
    load().catch(e=>{if(!ctrl.signal.aborted)setError(e.message);});
    return ()=>ctrl.abort();
  },[]);
  async function run(fn) {
    setBusy(true);setError("");
    try { await fn(); } catch(e){setError(e.message);} finally {setBusy(false);}
  }
  const book=books.find(b=>b.id+"/"+b.color===draft.book) ?? books[0];
  async function scan(request, previous) {
    const {id}=await api("coverage",request);
    const next=await api("coverage/"+id);
    setReport(next);setPreview(null);
    update({report:id,index:null,pgn:"",pending:null,previous:null});
    if(previous!=null)setMessage("Saved. Authored coverage moved from "+percent(previous)+" to "+percent(next.authored)+".");
  }
  function inputScan() {
    const game=new Chess();
    if(draft.line?.trim())game.loadPgn(draft.line);
    const history=game.history({verbose:true});
    return {repertoire:book.id,color:book.color,through:Number(draft.through??10),
      route:{fen:history[0]?.before??game.fen(),moves:history.map(m=>m.lan)}};
  }
  const gap=report?.gaps[draft.index];
  function open(index) {
    const link=lead(report,report.gaps[index]);
    window.open(link.url,"_blank","noopener,noreferrer");
    setPreview(null);setMessage("");update({index,pgn:"",pending:null});
  }
  async function save(command) {
    update({pending:command});
    await api("coverage/save",command,undefined,{requireJson:true});
    setMessage("Continuation saved. Refreshing coverage…");
    update({pending:null,previous:report.authored});setPreview(null);
    await scan(report.scan,report.authored);
  }
  const locked=busy || !!draft.pending;
  const edit={report:report?.id,gap:draft.index,pgn:draft.pgn??""};
  return <section><h2>Repertoire gaps</h2>
    <fieldset disabled={locked}><label>Repertoire <select value={book ? book.id+"/"+book.color : ""} onChange={e=>update({book:e.target.value})}>
      {books.map(b=><option key={b.id+b.color} value={b.id+"/"+b.color}>{b.example_line} ({b.color})</option>)}</select></label>
      <label>Starting line (PGN) <input placeholder="1. e4 e5 2. Nf3 Nc6 3. Bc4" value={draft.line??""} onChange={e=>update({line:e.target.value})}/></label>
      <label>Through my move <input type="number" min="1" max="100" value={draft.through??10} onChange={e=>update({through:e.target.value})}/></label>
      <button disabled={!book} onClick={()=>run(()=>scan(inputScan()))}>Scan repertoire</button></fieldset>
    {busy && <p role="status">Working…</p>}{error && <p role="alert">{error}</p>}{message && <p role="status">{message}</p>}
    {report && <><h3>{books.find(b=>b.id===report.scan.repertoire&&b.color===report.scan.color)?.example_line} · {report.scan.color}</h3>
      <p>From {notation(position(report.scan.route))||"the starting position"} through your move {report.scan.through} · Maia-2 Rapid 1500/1500</p>
      <p>Authored {range(report.authored,report.unresolved)} · Personally studied {range(report.studied,report.unresolved)}</p>
      <p>Ended early {percent(report.ended)} · Unresolved {percent(report.unresolved)} (conflicting replies {percent(report.conflict)})</p>
      {(report.stale||error||draft.previous!=null) && <button disabled={locked} onClick={()=>run(()=>scan(report.scan,draft.previous))}>Retry scan</button>}
      {report.gaps.length>0 && <button disabled={locked||report.stale} onClick={()=>open(0)}>Work on next gap</button>}
      {gap && <section><h3>Your {report.scan.color} reply</h3><p>{lead(report,gap).text}</p>
        <a href={lead(report,gap).url} target="_blank" rel="noreferrer">Open line in Lichess</a>
        <label>Paste PGN from Lichess <textarea disabled={locked} value={draft.pgn??""} onChange={e=>{setPreview(null);update({pgn:e.target.value});}}/></label>
        <button disabled={locked||report.stale||!edit.pgn} onClick={()=>run(async()=>setPreview(await api("coverage/preview",edit)))}>Preview</button>
        {preview && <><ul>{preview.responses.map(r=><li key={r.position}>{r.move} · {r.status}</li>)}</ul>
          <button disabled={locked||preview.responses.some(r=>r.status==="conflict")} onClick={()=>run(()=>save({draft:edit,command:preview.command}))}>Save to repertoire</button></>}
        {draft.pending && <button disabled={busy} onClick={()=>run(()=>save(draft.pending))}>Retry same save</button>}
      </section>}
      <ol>{report.gaps.slice(0,draft.visible??30).map((g,i)=><li key={g.fen}><button disabled={locked||report.stale} onClick={()=>open(i)}>{percent(g.probability)} · {lead(report,g).text}</button></li>)}</ol>
      {report.gaps.length>(draft.visible??30) && <button onClick={()=>update({visible:(draft.visible??30)+30})}>Show more gaps</button>}
    </>}
  </section>;
}
