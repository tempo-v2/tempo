import React, { useEffect, useRef, useState } from "react";
import { api } from "./api.js";
import { decodePuzzle, PuzzleBoard, recoverPuzzleAction, startPuzzle } from "./puzzle_play.jsx";

const themeNames = {
  mix: "Healthy mix", mateIn5: "Mate in 5 or more", attackingF2F7: "Attacking f2 or f7",
  xRayAttack: "X-ray attack", masterVsMaster: "Master vs. Master", superGM: "Super GM",
  morphysMate: "Morphy’s mate", pillsburysMate: "Pillsbury’s mate",
  vukovicMate: "Vuković mate", anastasiaMate: "Anastasia’s mate",
  bodenMate: "Boden’s mate", swallowstailMate: "Swallow’s tail mate",
};
function name(key) {
  const label = key.replace(/([a-z])([A-Z0-9])/g, "$1 $2").toLowerCase();
  return themeNames[key] ?? label[0].toUpperCase() + label.slice(1);
}
const band = (pack) => pack.ceiling === null ? `${pack.band}+` : `${pack.band}–${pack.ceiling}`;
const packsUrl = "tactics/packs";
const playsUrl = "tactics/plays";

export function TacticPacks({ sounds }) {
  const [index, setIndex] = useState(null);
  const [theme, setTheme] = useState("hangingPiece");
  const [packs, setPacks] = useState(null);
  const [morePacks, setMorePacks] = useState(false);
  const [selected, setSelected] = useState(null);
  const [members, setMembers] = useState(null);
  const [pendingPlays, setPendingPlays] = useState([]);
  const [view, setView] = useState(null);
  const [issue, setIssue] = useState("");
  const [busy, setBusy] = useState(false);
  const running = useRef(false);
  const mounted = useRef(false);
  const retry = useRef(null);
  const [refresh, setRefresh] = useState(0);

  async function readIndex() {
    const next = await api(packsUrl);
    if (index && index.revision !== next.revision) {
      setSelected(null); setPacks(null); setMorePacks(false); setMembers(null);
    }
    setIndex(next);
  }
  async function resume(id) { return decodePuzzle(await api(`${playsUrl}/${id}`), "tactics"); }
  async function readPending(after = 0) {
    const page = await api(`${playsUrl}?after=${after}`);
    setPendingPlays((old) => after ? [...old, ...page] : page);
  }
  async function run(work) {
    if (running.current) return;
    running.current = true;
    retry.current = work;
    setBusy(true);
    setIssue("");
    try { await work(); retry.current = null; }
    catch (error) { if (mounted.current) setIssue(error.message); }
    finally { running.current = false; if (mounted.current) setBusy(false); }
  }
  useEffect(() => {
    mounted.current = true;
    void run(async () => {
      const restored = await recoverPuzzleAction("tactics");
      if (!mounted.current) return;
      if (restored) setView(await resume(restored.play));
      await readIndex();
    });
    return () => { mounted.current = false; };
  }, []);
  useEffect(() => {
    if (!index?.revision || view) return;
    const request = new AbortController();
    async function load() {
      const next = await api(`${packsUrl}/${encodeURIComponent(theme)}`, undefined, request.signal);
      if (next.revision !== index.revision) { await readIndex(); return; }
      setPacks((old) => [...next.packs, ...(old ?? []).filter((pack) => pack.band > next.packs.at(-1)?.band)]);
      if (!packs || packs.length <= 50) setMorePacks(next.packs.length === 50);
      await readPending();
    }
    load().catch((error) => {
      if (!request.signal.aborted) { setIssue(error.message); retry.current = load; }
    });
    return () => request.abort();
  }, [index, theme, refresh, Boolean(view)]);
  useEffect(() => {
    if (selected === null || view || !index?.revision) return;
    const request = new AbortController();
    async function load() {
      const reply = await api(`${packsUrl}/${encodeURIComponent(theme)}/${selected}`, undefined, request.signal);
      if (reply.revision !== index.revision) { await readIndex(); return; }
      setMembers(reply.puzzles);
    }
    load().catch((error) => {
      if (!request.signal.aborted) { setIssue(error.message); retry.current = load; }
    });
    return () => request.abort();
  }, [selected, theme, index, refresh, Boolean(view)]);
  function choose(pack) {
    setSelected(pack.band);
    setMembers(null);
    setIssue("");
  }
  function showView(next) {
    setView(next);
    if (next.mode !== "first" || !next.result) return;
    if (members?.some((item) => item.puzzle === next.puzzle && !item.done)) {
      setPacks((old) => old?.map((pack) => pack.band === selected ? { ...pack, done: pack.done + 1 } : pack));
    }
    setMembers((old) => old?.map((item) => item.puzzle === next.puzzle ? { ...item, done: true, resume: false } : item));
    setPendingPlays((old) => old.filter((item) => item.play !== next.play));
  }
  async function loadMorePacks() {
    const next = await api(`${packsUrl}/${encodeURIComponent(theme)}?after=${packs.at(-1).band}`);
    if (next.revision !== index.revision) { await readIndex(); return; }
    setPacks((old) => [...old, ...next.packs]);
    setMorePacks(next.packs.length === 50);
  }
  function tryPuzzle(member) {
    const intent = { puzzle: member.puzzle, practice: member.done, request: crypto.randomUUID(),
      dataset: index.dataset, revision: index.revision };
    void run(async () => {
      const reply = await startPuzzle(intent);
      if (!mounted.current) return;
      if (reply) showView(reply);
      else { setView(null); await readIndex(); setRefresh((value) => value + 1); }
    });
  }
  const nextMember = members?.find((item) => item.resume && !item.done) ?? members?.find((item) => !item.done);
  const following = members?.find((item) => item.puzzle !== view?.puzzle && item.resume && !item.done)
    ?? members?.find((item) => item.puzzle !== view?.puzzle && !item.done);
  const recommended = packs?.find((pack) => pack.done < 20);
  const selectedPack = packs?.find((pack) => pack.band === selected);
  const nextPack = selectedPack && packs.find((pack) => pack.band > selected && pack.done < 20);
  function openNextPack() {
    setView(null);
    choose(nextPack);
    setRefresh((value) => value + 1);
  }
  const nextPackButton = nextPack && <button disabled={busy} onClick={openNextPack}>Open next pack {band(nextPack)}</button>;
  if (view) return <PuzzleBoard key={view.play} view={view} scope="tactics" sounds={sounds} onView={showView}
    onRefresh={async () => showView(await resume(view.play))}
    heading={<>
      <button disabled={busy} onClick={() => { setView(null); setRefresh((value) => value + 1); }}>{selectedPack ? "Back to pack" : "Back to packs"}</button>
      <h2>Lichess puzzle</h2>
    </>}
    continuation={view.result && view.mode === "first" && members && (following
      ? <button disabled={busy} onClick={() => tryPuzzle(following)}>Next puzzle</button>
      : nextPackButton)}>
    {issue && <p role="alert">{issue} <button disabled={busy} onClick={() => run(retry.current)}>Retry</button></p>}
  </PuzzleBoard>;
  return <section aria-label="Puzzle packs">
    {issue && <p role="alert">{issue} <button disabled={busy} onClick={() => run(retry.current)}>Retry</button></p>}
    {index?.revision === null && <p>Puzzle packs have not been imported.</p>}
    {index?.revision && <>
      <label>Theme <select value={theme} onChange={(event) => {
        setTheme(event.target.value); setSelected(null); setMembers(null); setPacks(null); setIssue("");
      }}>
        {index.themes.map(({ key }) => <option key={key} value={key}>{name(key)}</option>)}
      </select></label>
      {selectedPack ? <div>
        <button onClick={() => { setSelected(null); setMembers(null); }}>All packs</button>
        <h2>{band(selectedPack)}</h2>
        {members && <>
          {nextMember
            ? <button disabled={busy} onClick={() => tryPuzzle(nextMember)}>{nextMember.resume ? "Resume puzzle" : "Play next puzzle"}</button>
            : nextPackButton}
          <ul className="items">{members.map((member, position) => <li key={member.puzzle}>
            <button disabled={busy || (!member.done && member.puzzle !== nextMember?.puzzle)}
              onClick={() => tryPuzzle(member)}>
              Puzzle {position + 1} <small>{member.rating}{member.done ? " · Practice" : ""}</small>
            </button>
          </li>)}</ul>
        </>}
      </div> : <>
        {recommended && <button onClick={() => choose(recommended)}>Open recommended pack {band(recommended)}</button>}
        <ul className="items">{packs?.map((pack) => <li key={pack.band}>
          <button onClick={() => choose(pack)}>{band(pack)} <small>{pack.done}/20</small></button>
        </li>)}</ul>
        {morePacks && <button disabled={busy} onClick={() => run(loadMorePacks)}>More packs</button>}
      </>}
      {pendingPlays.length > 0 && <>
        <h2>In progress</h2>
        <ul className="items">{pendingPlays.map((item) => <li key={item.play}>
          <button disabled={busy} onClick={() => run(async () => showView(await resume(item.play)))}>Resume {item.puzzle}</button>
        </li>)}</ul>
        {pendingPlays.length % 50 === 0 && <button disabled={busy} onClick={() => run(() => readPending(pendingPlays.at(-1).play))}>More in-progress puzzles</button>}
      </>}
    </>}
  </section>;
}
