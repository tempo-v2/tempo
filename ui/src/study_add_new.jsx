import React, { useEffect, useRef, useState } from "react";
import { api, readView } from "./api.js";

const journalKey = "tempo.study.add-new";
const labels = { opening: "opening moves", tactic: "tactics" };

function readPending() {
  try {
    const command = JSON.parse(sessionStorage.getItem(journalKey));
    return /^[0-9a-f]{32}$/.test(command?.request) && Object.hasOwn(labels, command.kind)
      ? command : null;
  } catch {
    return null;
  }
}

export function StudyAddNew({ day, dataset, refreshCounts }) {
  const [open, setOpen] = useState(false);
  const [kind, setKind] = useState("opening");
  const [quantity, setQuantity] = useState("5");
  const [command, setCommand] = useState(readPending);
  const [retry, setRetry] = useState(0);
  const [failed, setFailed] = useState(false);
  const [status, setStatus] = useState("");
  const current = useRef(command);

  useEffect(() => {
    if (!command) return;
    const controller = new AbortController();
    let timer;
    let inFlight = false;
    let admitted = false;
    const path = "study/add-new";

    function complete(message) {
      // The receipt is terminal. A later form submission gets a fresh identity.
      try { sessionStorage.removeItem(journalKey); } catch { /* The receipt already settled. */ }
      current.current = null;
      setCommand(null);
      setFailed(false);
      setStatus(message);
      refreshCounts.current?.();
    }

    async function deliver() {
      if (inFlight || document.hidden || controller.signal.aborted) return;
      inFlight = true;
      try {
        const reply = admitted
          ? await readView(path + "/" + command.request, controller.signal)
          : await api(path, command, controller.signal, { requireJson: true });
        if (controller.signal.aborted) return;
        if (reply?.status === "pending") {
          admitted = true;
          timer = setTimeout(deliver, 750);
          return;
        }
        switch (reply?.status) {
          case "added": {
            if (!Number.isInteger(reply.added) || reply.added < 0 || reply.added > command.quantity) break;
            const type = reply.added === 1 && command.quantity === 1
              ? (command.kind === "opening" ? "opening move" : "tactic")
              : labels[command.kind];
            const count = reply.added === command.quantity ? reply.added : `${reply.added} of ${command.quantity}`;
            complete(`Added ${count} ${type}.`);
            return;
          }
          case "paused":
            complete("New opening moves are paused.");
            return;
          case "expired":
            complete("That study day ended. Submit again for today.");
            return;
          case "unavailable":
            complete("The queue could not prepare those cards. Submit again to try once more.");
            return;
          case "conflict":
            complete("The request no longer matches this Study queue.");
            return;
        }
        throw new Error("Incomplete Study addition reply");
      } catch {
        if (!controller.signal.aborted) {
          setFailed(true);
          setStatus("The addition could not be confirmed.");
        }
      } finally {
        inFlight = false;
      }
    }
    const onVisibility = () => {
      clearTimeout(timer);
      if (!document.hidden) void deliver();
    };
    document.addEventListener("visibilitychange", onVisibility);
    void deliver();
    return () => {
      controller.abort();
      clearTimeout(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [command, retry, refreshCounts]);

  function submit(event) {
    event.preventDefault();
    if (current.current || !day || !dataset) return;
    const amount = Number(quantity);
    if (!Number.isInteger(amount) || amount < 1 || amount > 4294967295) return;
    const request = crypto.randomUUID().replaceAll("-", "");
    const intent = { request, dataset, day, kind, quantity: amount };
    try {
      sessionStorage.setItem(journalKey, JSON.stringify(intent));
    } catch {
      setStatus("Tempo could not save this request for retry.");
      return;
    }
    current.current = intent;
    setStatus("Preparing additional cards…");
    setFailed(false);
    setCommand(intent);
    setOpen(false);
  }

  return <div>
    <button type="button" aria-expanded={open} disabled={!day || !dataset || !!command}
      onClick={() => setOpen(value => !value)}>Add more</button>
    {open && <form aria-label="Add cards for today" onSubmit={submit}>
      <label>Card type <select value={kind} onChange={event => setKind(event.target.value)}>
        <option value="opening">Opening moves</option>
        <option value="tactic">Tactics</option>
      </select></label>
      <label>Extra cards today <input type="number" required min="1" max="4294967295" step="1"
        value={quantity} onChange={event => setQuantity(event.target.value)} /></label>
      <button type="submit">Add cards</button>
    </form>}
    {(command || status) && <span role="status">{status || "Preparing additional cards…"}</span>}
    {command && failed && <button type="button" onClick={() => {
      setFailed(false);
      setStatus("Checking your addition…");
      setRetry(value => value + 1);
    }}>Retry addition</button>}
  </div>;
}
