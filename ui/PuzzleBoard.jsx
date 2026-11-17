import React, { forwardRef, useMemo } from "react";
import { Board } from "@tempo-v2/core-components";
export const identity = (v) =>
  `${v.dataset_id}:${v.attempt_id}:${v.content_version}`;
export const turnFromFen = (fen) => fen.split(/\s+/)[1] === "b" ? "black" : "white";

export const PuzzleBoard = forwardRef(function PuzzleBoard(
  { view, locked, retainMoveOnLock, onMove },
  ref,
) {
  const destinations = useMemo(() => {
    const m = new Map();
    for (const uci of view.legal_moves) {
      const a = uci.slice(0, 2),
        b = uci.slice(2, 4);
      if (!m.has(a)) m.set(a, []);
      if (!m.get(a).includes(b)) m.get(a).push(b);
    }
    return m;
  }, [view.legal_moves]);
  const turn = turnFromFen(view.display_fen);
  const correction = view.feedback?.correction_uci;
  return (
    <Board
      ref={ref}
      fen={view.display_fen}
      orientation={view.learner_color}
      turn={turn}
      check={view.in_check ? turn : false}
      mode="legal"
      destinations={destinations}
      positionIdentity={identity(view)}
      positionRevision={view.revision}
      locked={locked || view.status !== "ready"}
      retainMoveOnLock={retainMoveOnLock}
      coordinates="edges"
      maxSize={Number.MAX_SAFE_INTEGER}
      shapes={correction ? [{ orig: correction.slice(0, 2), dest: correction.slice(2, 4), brush: "blue" }] : undefined}
      onMove={onMove}
      ariaLabel="Tactics chessboard"
      ariaDescribedBy={view.status === "ready" ? "puzzle-turn" : undefined}
    />
  );
});
