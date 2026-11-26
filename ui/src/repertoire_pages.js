import { useCallback, useEffect, useRef, useState } from "react";
import { readView } from "./api.js";

export const repertoireKey = ({ id, color }) => id + "/" + color;

export function useRepertoirePages(reload) {
  const [rows, setRows] = useState([]);
  const [loaded, setLoaded] = useState(false);
  const [more, setMore] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const controller = useRef(null);
  const load = useCallback((append = false) => controller.current?.(append), []);

  useEffect(() => {
    let active = true;
    let request;
    let refresh = true;
    let tail;
    async function read(append = false) {
      if (!active) return;
      if (!append) refresh = true;
      if (request || document.hidden || (append && !tail)) return;
      const current = new AbortController();
      request = current;
      const appendPage = append && !refresh;
      refresh = false;
      setBusy(true);
      try {
        const query = appendPage ? "?" + new URLSearchParams({ after: tail.id, color: tail.color }) : "";
        const page = await readView("openings/repertoires" + query, current.signal);
        if (!active || current.signal.aborted || refresh) return;
        tail = page.at(-1);
        setRows(prior => appendPage ? [...prior, ...page] : page);
        setLoaded(true);
        setMore(page.length === 50);
        setError(null);
      } catch (failure) {
        if (active && !current.signal.aborted && !refresh) {
          setError({ message: failure.message, append: appendPage });
        }
      } finally {
        request = undefined;
        if (active) {
          setBusy(false);
          if (refresh && !document.hidden) void read();
        }
      }
    }
    function visibilityChanged() {
      if (document.hidden) {
        if (request) refresh = true;
        request?.abort();
      } else if (refresh) {
        void read();
      }
    }
    controller.current = read;
    document.addEventListener("visibilitychange", visibilityChanged);
    void read();
    return () => {
      active = false;
      controller.current = null;
      request?.abort();
      document.removeEventListener("visibilitychange", visibilityChanged);
    };
  }, [reload]);

  return { rows, loaded, more, busy, error, load };
}
