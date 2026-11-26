import React, { useEffect, useRef, useState } from "react";
import { api, readView, visibleRead } from "./api.js";

const pollInterval = 2000;
const stateLabels = {
  queued: "Comparison queued.",
  running: "Comparing game…",
  failed: "Comparison failed.",
};
const unassessedLabels = {
  no_reply: "No repertoire reply at this position.",
  conflict: "The repertoire has an unresolved reply at this position.",
  unusable: "The saved repertoire reply is unusable at this position.",
};
const outcomeLabels = {
  followed: "Followed repertoire",
  differed: "Different from repertoire",
};
const filters = [
  ["differed", "Mismatches"],
  ["assessed", "All assessed"],
  ["unassessed", "Unassessed"],
];
const feedbackError = "The repertoire feedback reply was incomplete.";

function basisText(status, report) {
  if (status.current) return "Compared with your current repertoire.";
  if (status.revision !== report.repertoire_revision) return "Compared with an earlier version of your repertoire.";
  return "Compared with your current repertoire using an earlier comparison version.";
}

function gamePath(game) {
  return "games/archive/" + encodeURIComponent(game.id);
}

export function GameFeedback({ game, ply, onPly, retained, onSelect }) {
  const initialSelection = useRef(retained?.color === game.learner_color ? retained.id : undefined);
  const [selected, setSelected] = useState(null);
  const [query, setQuery] = useState({ after: "" });
  const [choices, setChoices] = useState([]);
  const [more, setMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState(null);

  useEffect(() => visibleRead(async (signal) => {
    setLoading(true);
    const params = new URLSearchParams({ after: query.after });
    if (!query.after && initialSelection.current) params.set("selected", initialSelection.current);
    try {
      const page = await readView(gamePath(game) + "/repertoires?" + params, signal);
      if (signal.aborted) return;
      const eligible = (choice) => choice?.color === game.learner_color &&
        typeof choice.id === "string" && typeof choice.example_line === "string";
      if (page?.game !== game.id || page.learner_color !== game.learner_color ||
          !Array.isArray(page.choices) || !page.choices.every(eligible) ||
          (page.selected != null && (!eligible(page.selected) || page.selected.id !== initialSelection.current)) ||
          typeof page.more !== "boolean") {
        throw new Error(feedbackError);
      }
      setChoices(prior => query.after ? [...prior, ...page.choices] : page.choices);
      setMore(page.more);
      setError(null);
      if (!query.after) {
        const choice = page.selected ?? (!page.more && page.choices.length === 1 ? page.choices[0] : null);
        setSelected(choice);
        if (choice) onSelect(choice);
      }
    } catch (failure) {
      if (!signal.aborted) setError(failure.message);
    } finally {
      if (!signal.aborted) setLoading(false);
    }
  }), [game, query, onSelect]);

  const available = selected && !choices.some(choice => choice.id === selected.id)
    ? [...choices, selected] : choices;
  return <section className="game-feedback" aria-label="Repertoire feedback">
    <h2>Repertoire feedback</h2>
    {available.length > 0 && <label>Repertoire
      <select value={selected?.id ?? ""} onChange={event => {
        const choice = available.find(entry => entry.id === event.target.value);
        setSelected(choice);
        onSelect(choice);
      }}>
        <option value="" disabled>Choose a repertoire</option>
        {available.map(entry => <option key={entry.id} value={entry.id}>{entry.example_line}</option>)}
      </select>
    </label>}
    {more && <button disabled={loading} onClick={() => setQuery({ after: choices.at(-1).id })}>More repertoires</button>}
    {error && <p className="error" role="alert">{error}{" "}
      <button onClick={() => setQuery({ ...query })}>Retry repertoires</button></p>}
    {!available.length && !error && <p>{loading ? "Loading repertoires…" : "No repertoire for this side."}</p>}
    {selected && <FeedbackResult key={selected.id} game={game} repertoire={selected.id} ply={ply} onPly={onPly} />}
  </section>;
}

function readReport(reply, game, repertoire, cached) {
  if (reply?.game !== game.id || reply.repertoire !== repertoire ||
      reply.learner_color !== game.learner_color || !Number.isSafeInteger(reply.revision) ||
      !["not_compared", "queued", "running", "failed", "complete"].includes(reply.state) ||
      typeof reply.current !== "boolean") throw new Error(feedbackError);
  if (reply.completed == null) {
    if (reply.current || reply.state === "complete") throw new Error(feedbackError);
    return null;
  }

  const { generation, repertoire_revision, comparator_version } = reply.completed;
  if (!Number.isSafeInteger(generation) || generation <= 0) throw new Error(feedbackError);
  if (reply.report == null && cached?.status.completed?.generation === generation) return cached.report;
  const report = reply.report;
  if (report?.game !== game.id || report.repertoire !== repertoire ||
      report.learner_color !== game.learner_color || report.repertoire_revision !== repertoire_revision ||
      report.comparator_version !== comparator_version || !Array.isArray(report.decisions)) {
    throw new Error(feedbackError);
  }
  const counts = { followed: 0, differed: 0, unassessed: 0 };
  let prior = -1;
  for (const row of report.decisions) {
    const assessed = Object.hasOwn(outcomeLabels, row?.outcome);
    if (!Number.isInteger(row?.ply) || row.ply <= prior || row.ply >= game.route.moves.length ||
        !Number.isInteger(row.move_number) || row.move_number < 1 ||
        row.played?.uci !== game.route.moves[row.ply] || typeof row.played.san !== "string" ||
        (assessed ? typeof row.expected?.san !== "string" :
          row.outcome !== "unassessed" || !Object.hasOwn(unassessedLabels, row.reason))) {
      throw new Error(feedbackError);
    }
    prior = row.ply;
    counts[row.outcome]++;
  }
  if (Object.keys(counts).some(outcome => counts[outcome] !== report[outcome])) throw new Error(feedbackError);
  return report;
}

function FeedbackResult({ game, repertoire, ply, onPly }) {
  const path = gamePath(game) + "/feedback/" + encodeURIComponent(repertoire);
  const [view, setView] = useState(null);
  const [filter, setFilter] = useState("differed");
  const [readError, setReadError] = useState(null);
  const [commandError, setCommandError] = useState(null);
  const [sending, setSending] = useState(false);
  const controller = useRef(null);

  useEffect(() => {
    let active = true;
    let pending;
    let timer;
    let refresh = false;
    let cache;
    async function poll() {
      clearTimeout(timer);
      if (!active || document.hidden || pending) return;
      refresh = false;
      const request = new AbortController();
      pending = request;
      try {
        const generation = cache?.status.completed?.generation;
        const reply = await readView(path + (generation == null ? "" : "?known_result=" + generation), request.signal);
        if (!active || request.signal.aborted || refresh) return;
        cache = { status: reply, report: readReport(reply, game, repertoire, cache) };
        setView(cache);
        setReadError(null);
      } catch (failure) {
        if (active && !request.signal.aborted && !refresh &&
            (!cache || failure.code !== "view_unavailable")) setReadError(failure.message);
      } finally {
        pending = null;
        if (active && !document.hidden) timer = setTimeout(poll, refresh ? 0 : pollInterval);
      }
    }
    function invalidate() {
      refresh = true;
      void poll();
    }
    function visibilityChanged() {
      if (document.hidden) {
        clearTimeout(timer);
        pending?.abort();
      } else {
        invalidate();
      }
    }
    const current = { invalidate };
    controller.current = current;
    document.addEventListener("visibilitychange", visibilityChanged);
    void poll();
    return () => {
      active = false;
      controller.current = null;
      clearTimeout(timer);
      pending?.abort();
      document.removeEventListener("visibilitychange", visibilityChanged);
    };
  }, [game, path, repertoire]);

  async function compare() {
    const expected = controller.current;
    if (!expected || !view || sending) return;
    const { revision, generation } = view.status;
    setSending(true);
    setCommandError(null);
    try {
      const reply = await api(path, { revision, generation }, AbortSignal.timeout(6000), { requireJson: true });
      if (reply?.game !== game.id || reply.repertoire !== repertoire ||
          reply.learner_color !== game.learner_color) throw new Error(feedbackError);
    } catch (failure) {
      if (controller.current === expected) setCommandError({ revision, generation, message: failure.message });
    } finally {
      if (controller.current === expected) {
        setSending(false);
        expected.invalidate();
      }
    }
  }

  const status = view?.status;
  const report = view?.report;
  const assessed = report ? report.followed + report.differed : 0;
  const first = report?.decisions.find(row => row.outcome === "differed");
  const position = report?.decisions.find(row => row.ply === ply);
  const rows = report?.decisions.filter(row => filter === "assessed"
    ? row.outcome !== "unassessed" : row.outcome === filter);
  const action = status?.state === "failed" ? "Retry comparison" :
    status?.state === "not_compared" ? "Compare game" :
    status?.state === "complete" && !status.current ? "Refresh comparison" : null;
  const error = commandError && commandError.generation === status?.generation &&
    commandError.revision === status?.revision ? commandError.message : readError;

  return <>
    {!status && !error && <p>Loading comparison…</p>}
    {status?.state === "not_compared" && <p>{status.legacy_only ?
      "Earlier mismatch findings are incomplete." : "No saved comparison."}</p>}
    {Object.hasOwn(stateLabels, status?.state) && <p role="status">{stateLabels[status.state]}</p>}
    {status?.state === "failed" && status.error && <p className="error">{status.error}</p>}
    {error && <p className="error" role="alert">{error}</p>}
    {action && <button disabled={sending} onClick={compare}>{sending ? "Requesting comparison…" : action}</button>}
    {report && <>
      <p><strong>{assessed ? `${report.followed} of ${assessed} repertoire decisions followed` :
        "No comparable repertoire decisions."}{assessed > 0 && ` — ${(100 * report.followed / assessed).toFixed(1)}%`}</strong></p>
      <p>{report.differed} {report.differed === 1 ? "mismatch" : "mismatches"}.{" "}
        {report.unassessed} unassessed {report.unassessed === 1 ? "learner move" : "learner moves"}.</p>
      <p>{basisText(status, report)}</p>
      {position && <p className="feedback-position"><strong>At this position</strong>{" "}
        <DecisionLabel row={position} color={game.learner_color} /></p>}
      <div className="controls feedback-controls">
        {first && <button onClick={() => onPly(first.ply)}>First repertoire mismatch</button>}
        {filters.map(([value, label]) => <button key={value} aria-pressed={filter === value}
          onClick={() => setFilter(value)}>{label}</button>)}
      </div>
      {rows.length === 0 && filter === "differed" && assessed > 0 && <p>No repertoire mismatches.</p>}
      <ol className="items feedback-decisions" aria-label="Repertoire decisions">
        {rows.map(row => <li key={row.ply}>
          <DecisionLabel row={row} color={game.learner_color} />
          <button aria-label={`View position at ${moveNumber(row, game.learner_color)} ${row.played.san}`}
            onClick={() => onPly(row.ply)}>View position</button>
        </li>)}
      </ol>
    </>}
  </>;
}

function moveNumber(row, color) {
  return row.move_number + (color === "black" ? "..." : ".");
}

function DecisionLabel({ row, color }) {
  return <span><strong>{moveNumber(row, color)}</strong>{" "}Played {row.played.san}
    {row.outcome === "unassessed" ? ` · ${unassessedLabels[row.reason]}` :
      ` · Repertoire ${row.expected.san} · ${outcomeLabels[row.outcome]}`}
  </span>;
}
