import React from "react";
import { Button } from "@tempo-v2/core-components";

export function PuzzleActions({ view, state, actions }) {
  const recoveryBlocked = state.issue?.startsWith("Recovery storage");
  if (!view && (!state.issue || recoveryBlocked) && !state.canRetry) return null;
  return (
    <div className="actions">
      {state.canRetry ? (
        <Button variant="primary" onClick={actions.retry}>Retry</Button>
      ) : state.issue === "refresh" ? (
        <Button disabled={state.busy} onClick={actions.refresh}>Refresh attempt</Button>
      ) : view?.status === "ready" ? (
        <Button disabled={state.busy || recoveryBlocked} onClick={actions.abandon}>
          Abandon
        </Button>
      ) : (
        <Button variant="primary" disabled={state.busy || recoveryBlocked} onClick={actions.next}>
          {view?.status === "empty" ? "Check again" : "Next puzzle"}
        </Button>
      )}
    </div>
  );
}
