import React from "react";

export function ExerciseLayout({ label, board, children }) {
  return <section className="study" aria-label={label}>
    <div className="study-board">{board}</div>
    <div className="study-panel">{children}</div>
  </section>;
}
