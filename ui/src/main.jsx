import React, { useEffect, useLayoutEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import "@tempo-v2/core-components/styles.css";
import "./style.css";
import { Study } from "./study.jsx";
import { Openings, Tactics } from "./library.jsx";
import { Games } from "./games.jsx";
import { Settings } from "./settings.jsx";
import { TacticCapture } from "./tactic_capture.jsx";
import { createBoardSounds } from "./sounds.js";

import { Gaps } from "./gaps.jsx";

const tabs = ["Study", "Openings", "Tactics", "Games", "Settings"];

function App() {
  const [sounds] = useState(createBoardSounds);
  const [page, setPage] = useState("Study");
  const [openingTab,setOpeningTab] = useState("Library");
  const [captureOpener, setCaptureOpener] = useState(null);
  const [captured, setCaptured] = useState(null);
  const [game, setGame] = useState(null);
  const [draft, setDraft] = useState(null);
  useEffect(() => { sounds.preload(); }, [sounds]);
  useLayoutEffect(() => sounds.cancel, [sounds, page, openingTab]);
  function openGame(id, ply) { setGame({ id, ply }); setPage("Games"); }
  function openCapture(event) { setCaptureOpener(event.currentTarget); }
  function closeCapture() {
    setCaptureOpener(null);
    requestAnimationFrame(() => captureOpener?.focus({ preventScroll: true }));
  }
  function addOpening(route) { setOpeningTab("Library"); setDraft(route); setPage("Openings"); }
  return <div onPointerDownCapture={sounds.unlock} onPointerUpCapture={sounds.unlock} onKeyDownCapture={sounds.unlock}>
    <header><div className="header-actions"><strong>Tempo</strong>
      <button className="primary" onClick={openCapture}>Capture</button>
    </div><nav aria-label="Main navigation">
      {tabs.map((tab) => <button key={tab} onClick={() => setPage(tab)}
        aria-current={page === tab ? "page" : undefined}>{tab}</button>)}
    </nav></header>
    <main>
      {page === "Study" && <Study openGame={openGame} sounds={sounds} overlayOpen={Boolean(captureOpener)} />}
      {page === "Openings" && <nav aria-label="Openings">{["Library","Gaps"].map(t=><button key={t} aria-pressed={openingTab===t} onClick={()=>setOpeningTab(t)}>{t}</button>)}</nav>}
      {page === "Openings" && openingTab==="Gaps" && <Gaps/>}
      {page === "Openings" && openingTab==="Library" && <Openings sounds={sounds} fromGame={draft} onSaved={() => setDraft(null)} />}
      {page === "Tactics" && <Tactics sounds={sounds} openGame={openGame} openCapture={openCapture} captured={captured} />}
      {page === "Games" && <Games sounds={sounds} jump={game} addOpening={addOpening} settings={() => setPage("Settings")} />}
      {page === "Settings" && <Settings sounds={sounds} />}
    </main>
    {captureOpener && <TacticCapture onSaved={setCaptured} onClose={closeCapture} />}
  </div>;
}
createRoot(document.getElementById("root")).render(<App />);
