import React from "react";
import { Button } from "@tempo-v2/core-components";
export function PuzzleActions({ view, state, actions }) {
  return (
    <div className="actions">
      {state.canRetry && (
        <Button variant="primary" onClick={actions.retry}>
          Retry
        </Button>
      )}
      {state.issue === "refresh" && (
        <Button onClick={actions.refresh}>Refresh attempt</Button>
      )}
      {view?.status === "ready" && (
        <Button
          disabled={state.busy || state.canRetry || state.issue === "refresh"}
          onClick={actions.abandon}
        >
          Abandon
        </Button>
      )}
      <Button
        variant="primary"
        disabled={
          state.busy ||
          state.canRetry ||
          (state.issue && state.issue.startsWith("Recovery storage"))
        }
        onClick={actions.next}
      >
        Next
      </Button>
    </div>
  );
}
