import React, { useEffect, useId, useRef, useState } from "react";
import { api } from "./api.js";
import { ChessBoard, position } from "./board.jsx";
import { ExerciseLayout } from "./exercise_layout.jsx";
import { decodePuzzle, PuzzleBoard, recoverPuzzleActions, startPuzzle } from "./puzzle_play.jsx";

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
const startingPosition = position({ moves: [] });

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
  const [packsIssue, setPacksIssue] = useState("");
  const [membersIssue, setMembersIssue] = useState("");
  const [busy, setBusy] = useState(false);
  const running = useRef(false);
  const mounted = useRef(false);
  const currentView = useRef(view);
  currentView.current = view;
  const retry = useRef(null);
  const [refresh, setRefresh] = useState(0);
  const [retryPacks, setRetryPacks] = useState(0);
  const [retryMembers, setRetryMembers] = useState(0);
  const panelPrefix = useId();
  const expandedHeader = useRef(null);
  const themePicker = useRef(null);
  const returnFocus = useRef(false);
  const playing = Boolean(view);

  async function readIndex(signal) {
    const next = await api(packsUrl, undefined, signal);
    if (index && index.revision !== next.revision) {
      setSelected(null); setPacks(null); setMorePacks(false); setMembers(null);
    }
    setIndex(next);
  }
  async function resume(id, dataset) {
    const reply = await api(`${playsUrl}/${id}`);
    return decodePuzzle(reply, "tactics", dataset === undefined ? undefined : { dataset, play: id });
  }
  async function readPending(after = 0, signal) {
    const page = await api(`${playsUrl}?after=${after}`, undefined, signal);
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
    let recovered = null;
    let multiple = false;
    async function readAfterRecovery() {
      if (recovered && !multiple) {
        const view = await resume(recovered.play, recovered.dataset);
        if (!mounted.current) return;
        setView(view);
      }
      await readIndex();
    }
    void run(async () => {
      await recoverPuzzleActions("tactics", (reply) => {
        multiple ||= Boolean(recovered && (recovered.play !== reply.play || recovered.dataset !== reply.dataset));
        recovered = reply;
      });
      if (!mounted.current) return;
      retry.current = readAfterRecovery;
      await readAfterRecovery();
    });
    return () => { mounted.current = false; };
  }, []);
  useEffect(() => {
    if (!index?.revision || playing) return;
    const request = new AbortController();
    setPacksIssue("");
    async function load() {
      const next = await api(`${packsUrl}/${encodeURIComponent(theme)}`, undefined, request.signal);
      if (next.revision !== index.revision) { await readIndex(request.signal); return; }
      setPacks((old) => [...next.packs, ...(old ?? []).filter((pack) => pack.band > next.packs.at(-1)?.band)]);
      if (!packs || packs.length <= 50) setMorePacks(next.packs.length === 50);
      await readPending(0, request.signal);
    }
    load().catch((error) => {
      if (!request.signal.aborted) setPacksIssue(error.message);
    });
    return () => request.abort();
  }, [index, theme, refresh, retryPacks, playing]);
  useEffect(() => {
    if (selected === null || playing || !index?.revision) return;
    const request = new AbortController();
    setMembersIssue("");
    async function load() {
      const reply = await api(`${packsUrl}/${encodeURIComponent(theme)}/${selected}`, undefined, request.signal);
      if (reply.revision !== index.revision) { await readIndex(request.signal); return; }
      setMembers(reply.puzzles);
    }
    load().catch((error) => {
      if (!request.signal.aborted) setMembersIssue(error.message);
    });
    return () => request.abort();
  }, [selected, theme, index, refresh, retryMembers, playing]);
  useEffect(() => {
    if (!playing && returnFocus.current) {
      returnFocus.current = false;
      (expandedHeader.current ?? themePicker.current)?.focus();
    }
  }, [playing, selected]);
  function choose(band) {
    setSelected(band);
    setMembers(null);
    setMembersIssue("");
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
      else {
        setView(null);
        async function refreshPacks() {
          await readIndex();
          if (mounted.current) setRefresh((value) => value + 1);
        }
        retry.current = refreshPacks;
        await refreshPacks();
      }
    });
  }
  const nextMember = members?.find((item) => item.resume && !item.done) ?? members?.find((item) => !item.done);
  const following = members?.find((item) => item.puzzle !== view?.puzzle && item.resume && !item.done)
    ?? members?.find((item) => item.puzzle !== view?.puzzle && !item.done);
  const recommended = packs?.find((pack) => pack.done < 20);
  const selectedPack = packs?.find((pack) => pack.band === selected);
  const nextPack = selectedPack && packs.find((pack) => pack.band > selected && pack.done < 20);
  function returnToPacks() {
    returnFocus.current = true;
    setView(null);
    setRefresh((value) => value + 1);
  }
  function openNextPack() {
    choose(nextPack.band);
    returnToPacks();
  }
  const nextPackButton = nextPack && <button disabled={busy} onClick={openNextPack}>Open next pack {band(nextPack)}</button>;
  if (view) return <PuzzleBoard key={view.play} view={view} scope="tactics" sounds={sounds} onView={showView}
    onRefresh={async () => {
      const next = await resume(view.play, view.dataset);
      if (mounted.current && currentView.current === view) showView(next);
    }}
    heading={<>
      <button disabled={busy} onClick={returnToPacks}>{selectedPack ? "Back to pack" : "Back to packs"}</button>
      <h2>Lichess puzzle</h2>
    </>}
    continuation={view.result && view.mode === "first" && members && (following
      ? <button disabled={busy} onClick={() => tryPuzzle(following)}>Next puzzle</button>
      : nextPackButton)}>
    {issue && <p role="alert">{issue} <button disabled={busy} onClick={() => run(retry.current)}>Retry</button></p>}
  </PuzzleBoard>;
  return <ExerciseLayout label="Puzzle packs"
    board={<ChessBoard game={startingPosition} orientation="white" identity="tactics-pack-browser" />}>
    {issue && <p role="alert">{issue} <button disabled={busy} onClick={() => run(retry.current)}>Retry</button></p>}
    {!index && !issue && <p role="status">Loading puzzle packs…</p>}
    {index?.revision === null && <p>Puzzle packs have not been imported.</p>}
    {index?.revision && <>
      <div className="pack-theme">
        <label htmlFor={`${panelPrefix}-theme`}>Theme</label>
        <select id={`${panelPrefix}-theme`} ref={themePicker} value={theme} disabled={busy} onChange={(event) => {
          setTheme(event.target.value);
          choose(null);
          setPacks(null);
          setMorePacks(false);
          setPacksIssue("");
        }}>
          {index.themes.map(({ key }) => <option key={key} value={key}>{name(key)}</option>)}
        </select>
      </div>
      {pendingPlays.length > 0 && <details className="pack-progress">
        <summary>In progress</summary>
        <ul className="items">{pendingPlays.map((item) => <li key={item.play}>
          <button disabled={busy} onClick={() => run(async () => showView(await resume(item.play)))}>Resume {item.puzzle}</button>
        </li>)}</ul>
        {pendingPlays.length % 50 === 0 && <button disabled={busy} onClick={() => run(() => readPending(pendingPlays.at(-1).play))}>More in-progress puzzles</button>}
      </details>}
      {packsIssue && <p role="alert">{packsIssue} <button disabled={busy} onClick={() => setRetryPacks((value) => value + 1)}>Retry</button></p>}
      {!packs && !packsIssue && <p role="status">Loading rating bands…</p>}
      <div className="pack-accordion">{packs?.map((pack) => {
        const expanded = pack.band === selected;
        const panelId = `${panelPrefix}-${pack.band}`;
        return <div key={pack.band} className="pack-band">
          <h2><button className="pack-header" aria-expanded={expanded} aria-controls={panelId}
            ref={expanded ? expandedHeader : null} disabled={busy}
            onClick={() => choose(expanded ? null : pack.band)}>
            <span>{band(pack)}{pack === recommended && <small>Recommended</small>}</span>
            <small>{pack.done}/20</small>
          </button></h2>
          <div id={panelId} className="pack-body" hidden={!expanded}>{expanded && <>
            {membersIssue && <p role="alert">{membersIssue} <button disabled={busy} onClick={() => setRetryMembers((value) => value + 1)}>Retry</button></p>}
            {!members && !membersIssue && <p role="status">Loading puzzles…</p>}
            {members && <>
              {nextMember
                ? <button className="primary" disabled={busy} onClick={() => tryPuzzle(nextMember)}>{nextMember.resume ? "Resume puzzle" : "Play next puzzle"}</button>
                : nextPackButton}
              <ul className="items">{members.map((member, offset) => <li key={member.puzzle}>
                <button disabled={busy || (!member.done && member.puzzle !== nextMember?.puzzle)}
                  onClick={() => tryPuzzle(member)}>
                  Puzzle {offset + 1} <small>{member.rating}{member.done ? " · Practice" : ""}</small>
                </button>
              </li>)}</ul>
            </>}
          </>}</div>
        </div>;
      })}</div>
      {morePacks && <button disabled={busy} onClick={() => run(loadMorePacks)}>More packs</button>}
    </>}
  </ExerciseLayout>;
}
