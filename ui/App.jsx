import React, { useEffect, useRef, useState } from "react";
import { createStudyController } from "./study_controller.js";
import { TacticsPage } from "./TacticsPage.jsx";
import { identity } from "./PuzzleBoard.jsx";
const storage = {
  load: () =>
    JSON.parse(sessionStorage.getItem("tempo-study-recovery") || "null"),
  save: (e) =>
    sessionStorage.setItem("tempo-study-recovery", JSON.stringify(e)),
};
const transport = {
  async send(command) {
    const response = await fetch("/api/study", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(command),
    });
    if (!response.ok) throw new Error(`Server returned ${response.status}`);
    return response.json();
  },
};
export function App() {
  const [model, setModel] = useState({ view: null, state: { busy: false } }),
    [choice, setChoice] = useState(null);
  const board = useRef(null),
    controller = useRef(null),
    choiceRef = useRef(null);
  choiceRef.current = choice;
  if (!controller.current)
    controller.current = createStudyController({
      transport,
      storage,
      makeSubmissionId: () => crypto.randomUUID(),
      render: (view, state) => setModel({ view, state }),
    });
  useEffect(() => {
    controller.current.start().catch(() => {});
  }, []);
  useEffect(() => {
    const v = model.view;
    if (
      choiceRef.current &&
      (!v ||
        !["ready"].includes(v.status) ||
        model.state.busy ||
        model.state.canRetry ||
        identity(v) !== choiceRef.current.id ||
        v.revision !== choiceRef.current.revision)
    )
      setChoice(null);
    board.current?.clearInput();
  }, [model.view, model.state.canRetry, model.state.busy]);
  const run = (fn) => () => {
    choiceRef.current = null;
    setChoice(null);
    fn().catch(() => {});
  };
  const actions = {
    next: run(() => controller.current.next()),
    abandon: run(() => controller.current.abandon()),
    retry: run(() => controller.current.retry()),
    refresh: run(() => controller.current.refresh()),
  };
  function onMove(event) {
    const snapshot = controller.current.getState();
    const v = snapshot.view,
      state = snapshot;
    if (
      !v ||
      v.status !== "ready" ||
      state.busy ||
      state.canRetry ||
      state.issue === "refresh" ||
      event.positionIdentity !== identity(v) ||
      event.positionRevision !== v.revision
    )
      return;
    const base = event.from + event.to;
    const moves = v.legal_moves.filter((m) => m.startsWith(base));
    if (!moves.length) {
      board.current?.clearInput();
      return;
    }
    const promotions = moves.filter((m) => m.length === 5);
    if (promotions.length) {
      setChoice({
        id: identity(v),
        revision: v.revision,
        base,
        moves: promotions.map((m) => m[4]),
      });
      return;
    }
    controller.current.answer(base).catch(() => {});
  }
  function onChoose(origin, role) {
    const snapshot = controller.current.getState();
    const v = snapshot.view,
      state = snapshot;
    if (
      choiceRef.current !== origin ||
      !v ||
      v.status !== "ready" ||
      state.busy ||
      state.canRetry ||
      state.issue === "refresh" ||
      identity(v) !== origin.id ||
      v.revision !== origin.revision ||
      !v.legal_moves.includes(origin.base + role)
    )
      return;
    choiceRef.current = null;
    setChoice(null);
    controller.current.answer(origin.base + role).catch(() => {});
  }
  return (
    <TacticsPage
      {...model}
      actions={actions}
      boardRef={board}
      choice={choice}
      onMove={onMove}
      onChoose={onChoose}
      onCancel={() => {
        choiceRef.current = null;
        setChoice(null);
        board.current?.clearInput();
      }}
    />
  );
}
