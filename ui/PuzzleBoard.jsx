import React, { forwardRef, useMemo } from "react";
import { Board } from "@tempo-v2/core-components";
export const identity = (v) =>
  `${v.dataset_id}:${v.attempt_id}:${v.content_version}`;
export const PuzzleBoard = forwardRef(function PuzzleBoard(
  { view, locked, onMove },
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
  const turn = view.display_fen.split(" ")[1] === "b" ? "black" : "white";
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
      onMove={onMove}
      ariaLabel="Tactics chessboard"
    />
  );
});
