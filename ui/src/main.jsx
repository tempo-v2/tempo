import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import "@tempo-v2/core-components/styles.css";
import "./style.css";
import { Study } from "./study.jsx";
import { Openings, Tactics } from "./library.jsx";
import { Games, Settings } from "./games.jsx";

const tabs = ["Study", "Openings", "Tactics", "Games", "Settings"];

function App() {
  const [page, setPage] = useState("Study");
  const [game, setGame] = useState(null);
  const [draft, setDraft] = useState(null);
  function openGame(id, ply) { setGame({ id, ply }); setPage("Games"); }
  function addOpening(route) { setDraft(route); setPage("Openings"); }
  return <>
    <header><strong>Tempo</strong><nav aria-label="Main navigation">
      {tabs.map((tab) => <button key={tab} onClick={() => setPage(tab)}
        aria-current={page === tab ? "page" : undefined}>{tab}</button>)}
    </nav></header>
    <main>
      {page === "Study" && <Study openGame={openGame} />}
      {page === "Openings" && <Openings fromGame={draft} onSaved={() => setDraft(null)} />}
      {page === "Tactics" && <Tactics openGame={openGame} />}
      {page === "Games" && <Games jump={game} addOpening={addOpening} settings={() => setPage("Settings")} />}
      {page === "Settings" && <Settings />}
    </main>
  </>;
}
createRoot(document.getElementById("root")).render(<App />);
