import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import "@tempo-v2/core-components/styles.css";
import "./style.css";
import { Study } from "./study.jsx";
import { Openings, Tactics } from "./library.jsx";
import { Games } from "./games.jsx";
import { Settings } from "./settings.jsx";
import { createBoardSounds } from "./sounds.js";

import { Gaps } from "./gaps.jsx";

const tabs = ["Study", "Openings", "Tactics", "Games", "Settings"];

function App() {
  const [sounds] = useState(createBoardSounds);
  const [page, setPage] = useState("Study");
  const [openingTab,setOpeningTab] = useState("Library");
  const [game, setGame] = useState(null);
  const [draft, setDraft] = useState(null);
  function openGame(id, ply) { setGame({ id, ply }); setPage("Games"); }
  function addOpening(route) { setOpeningTab("Library"); setDraft(route); setPage("Openings"); }
  return <div onPointerDownCapture={sounds.unlock} onKeyDownCapture={sounds.unlock}>
    <header><strong>Tempo</strong><nav aria-label="Main navigation">
      {tabs.map((tab) => <button key={tab} onClick={() => setPage(tab)}
        aria-current={page === tab ? "page" : undefined}>{tab}</button>)}
    </nav></header>
    <main>
      {page === "Study" && <Study openGame={openGame} sounds={sounds} />}
      {page === "Openings" && <nav aria-label="Openings">{["Library","Gaps"].map(t=><button key={t} aria-pressed={openingTab===t} onClick={()=>setOpeningTab(t)}>{t}</button>)}</nav>}
      {page === "Openings" && openingTab==="Gaps" && <Gaps/>}
      {page === "Openings" && openingTab==="Library" && <Openings sounds={sounds} fromGame={draft} onSaved={() => setDraft(null)} />}
      {page === "Tactics" && <Tactics sounds={sounds} openGame={openGame} />}
      {page === "Games" && <Games sounds={sounds} jump={game} addOpening={addOpening} settings={() => setPage("Settings")} />}
      {page === "Settings" && <Settings sounds={sounds} />}
    </main>
  </div>;
}
createRoot(document.getElementById("root")).render(<App />);
