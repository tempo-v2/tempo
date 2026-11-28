import React from "react";

export function ReplacementImpact({ impact }) {
  return <ul className="gap-conflict-list">{impact.map((line, index) => <li key={index}>
    <strong>{line.name}</strong>
    <dl>
      <dt>Before</dt><dd>{line.before}</dd>
      <dt>After</dt><dd>{line.after ?? "Archived without an active continuation."}</dd>
    </dl>
    {line.stopped_at && <p>{line.stopped_at}</p>}
  </li>)}</ul>;
}
