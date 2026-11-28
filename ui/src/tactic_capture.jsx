import React, { useEffect, useLayoutEffect, useRef, useState } from "react";
import { api, fileText, readView } from "./api.js";
import { pendingTacticCapture, resolveTacticCapture, saveTacticCapture } from "./puzzle-journal.js";

const rejectedDraft = new Set(["capture_invalid", "capture_lookup_failed"]);

export function TacticCapture({ onSaved, onClose }) {
  const dialog = useRef(null);
  const editor = useRef(null);
  const fileRead = useRef(null);
  const preview = useRef(null);
  const previewRunning = useRef(null);
  const sending = useRef(false);
  const [dataset, setDataset] = useState(null);
  const [loadError, setLoadError] = useState("");
  const [loadRetry, setLoadRetry] = useState(0);
  const [input, setInput] = useState("");
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [formatChoice, setFormatChoice] = useState(null);
  const [setupChoice, setSetupChoice] = useState(null);
  const [detected, setDetected] = useState(null);
  const [optionsOpen, setOptionsOpen] = useState(false);
  const [checking, setChecking] = useState(false);
  const [pending, setPending] = useState(null);
  const [busy, setBusy] = useState(false);
  const [reading, setReading] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [focus, setFocus] = useState(0);

  const isId = /^[a-zA-Z0-9]{5}$/.test(input.trim());
  const detection = detected?.input === input && detected.url === url ? detected.options : null;
  const format = formatChoice ?? detection?.format ?? "generic_pgn";
  const setup = setupChoice ?? (format === "chess_com_puzzle");
  const settingsLocked = busy || Boolean(pending);

  useLayoutEffect(() => {
    const element = dialog.current;
    element.showModal();
    return () => element.close();
  }, []);

  useLayoutEffect(() => {
    if (!settingsLocked && !reading) editor.current?.focus({ preventScroll: true });
  }, [focus, settingsLocked, reading]);

  useEffect(() => {
    const controller = new AbortController();
    async function load() {
      setLoadError("");
      try {
        const { dataset } = await readView("tactics/capture-context", controller.signal);
        const command = await pendingTacticCapture(dataset);
        if (controller.signal.aborted) return;
        setDataset(dataset);
        if (command) {
          const draft = command.input;
          setPending(command);
          setInput(draft.pgn ?? draft.id);
          setName(draft.name ?? "");
          setUrl(draft.puzzle_url ?? "");
          setFormatChoice(draft.format ?? null);
          setSetupChoice(draft.first_move_is_setup ?? null);
        }
      } catch (failure) {
        if (!controller.signal.aborted) setLoadError(failure.message);
      }
    }
    void load();
    return () => controller.abort();
  }, [loadRetry]);

  useEffect(() => {
    if (!optionsOpen || settingsLocked || isId || (!input.trim() && !url.trim())) return;
    const controller = new AbortController();
    preview.current = controller;
    const timer = setTimeout(async () => {
      previewRunning.current = controller;
      setChecking(true);
      try {
        const options = await api("tactics/import-options", { pgn: input, puzzle_url: url }, controller.signal, { requireJson: true });
        setDetected({ input, url, options });
      } catch {
        // The save command validates the current draft with the same defaults.
      } finally {
        if (previewRunning.current === controller) {
          previewRunning.current = null;
          setChecking(false);
        }
      }
    }, 200);
    return () => {
      clearTimeout(timer);
      controller.abort();
      if (preview.current === controller) preview.current = null;
    };
  }, [input, url, isId, settingsLocked, optionsOpen]);

  useEffect(() => () => fileRead.current?.abort(), []);

  function changeInput(value) {
    fileRead.current?.abort();
    setReading(false);
    setInput(value);
    setError("");
    setNotice("");
  }

  async function upload(event) {
    if (!event.target.files?.length) return;
    fileRead.current?.abort();
    const current = new AbortController();
    fileRead.current = current;
    setReading(true);
    setError("");
    setNotice("");
    const read = fileText(event, setInput, setError, current.signal);
    event.target.value = "";
    try { await read; }
    finally {
      if (!current.signal.aborted) {
        fileRead.current = null;
        setReading(false);
      }
    }
  }

  async function save(event) {
    event.preventDefault();
    if (sending.current || previewRunning.current || reading || !dataset || !input.trim()) return;
    sending.current = true;
    setBusy(true);
    setError("");
    preview.current?.abort();
    const command = pending ?? {
      dataset, request: crypto.randomUUID(),
      input: isId ? { id: input.trim(), name } : {
        pgn: input, name, puzzle_url: url, format: formatChoice, first_move_is_setup: setupChoice,
      },
    };
    try {
      if (!pending) {
        await saveTacticCapture(command);
        setPending(command);
      }
      const endpoint = command.input.id === undefined ? "tactics/capture" : "tactics/capture/lichess";
      const result = await api(endpoint, command, undefined, { requireJson: true });
      if (result?.request !== command.request || !result.id || result.puzzle?.id !== result.id) {
        throw new Error("The save response could not be confirmed. Retry the same capture.");
      }
      setNotice("Saved");
      onSaved(result.puzzle);
      await resolveTacticCapture(command);
      setPending(null);
      setInput("");
      setName("");
      setUrl("");
      setFormatChoice(null);
      setSetupChoice(null);
      setDetected(null);
      setFocus((value) => value + 1);
    } catch (failure) {
      if (rejectedDraft.has(failure.code)
          || failure.code === "capture_conflict" || failure.code === "capture_dataset_changed") {
        try {
          await resolveTacticCapture(command);
          setPending(null);
          if (failure.code === "capture_dataset_changed") setDataset(null);
        } catch (journalFailure) {
          setError(journalFailure.message);
          return;
        }
      }
      setError(failure.message);
    } finally {
      sending.current = false;
      setBusy(false);
    }
  }

  return <dialog ref={dialog} className="tactic-capture" aria-labelledby="capture-title" onCancel={(event) => {
    event.preventDefault();
    if (!sending.current) onClose();
  }}>
    <div className="tactic-capture-heading">
      <h2 id="capture-title">Capture puzzle</h2>
      <button type="button" disabled={busy} onClick={onClose}>Close</button>
    </div>
    <form onSubmit={save}>
      <fieldset className="puzzle-import-fields" disabled={settingsLocked}>
        <label htmlFor="capture-paste">Puzzle PGN or Lichess puzzle ID</label>
        <textarea id="capture-paste" ref={editor} required rows={5} value={input} spellCheck={false}
          onChange={(event) => changeInput(event.target.value)} />
        <label>Upload puzzle PGN<input type="file" accept=".pgn,text/plain" onChange={upload} /></label>
        <details onToggle={(event) => setOptionsOpen(event.currentTarget.open)}>
          <summary>Import options</summary>
          <div className="puzzle-import-fields">
            <label>Puzzle name (optional)<input value={name} maxLength={160} onChange={(event) => setName(event.target.value)} /></label>
            {!isId && <>
              <label>Puzzle URL (optional)<input value={url} inputMode="url" maxLength={2048} onChange={(event) => setUrl(event.target.value)} /></label>
              <label>Import format<select value={format} onChange={(event) => setFormatChoice(event.target.value)}>
                <option value="generic_pgn">Generic PGN</option>
                <option value="chess_com_puzzle">Chess.com puzzle</option>
                <option value="lichess_pgn">Lichess PGN</option>
              </select></label>
              <label className="checkbox-label"><input type="checkbox" checked={setup}
                onChange={(event) => setSetupChoice(event.target.checked)} /> First move is setup</label>
            </>}
          </div>
        </details>
      </fieldset>
      {loadError && <p className="error" role="alert">{loadError}{" "}
        <button type="button" onClick={() => setLoadRetry((value) => value + 1)}>Retry opening Capture</button>
      </p>}
      {error && <p className="error" role="alert">{error}</p>}
      <div className="controls">
        <button className="primary" disabled={busy || checking || reading || !dataset || !input.trim()}>
          {busy ? "Saving…" : pending ? "Retry save" : "Save puzzle"}
        </button>
        {notice && <span role="status">{notice}</span>}
      </div>
    </form>
  </dialog>;
}
