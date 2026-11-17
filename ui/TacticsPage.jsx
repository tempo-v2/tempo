import React from "react";
import {
  PageHeader,
  SectionHeader,
  Surface,
  StatusNotice,
} from "@tempo-v2/core-components";
import { PuzzleBoard } from "./PuzzleBoard.jsx";
import { PuzzleFeedback } from "./PuzzleFeedback.jsx";
import { PuzzleActions } from "./PuzzleActions.jsx";
import { PromotionChooser } from "./PromotionChooser.jsx";
export function TacticsPage({
  view,
  state,
  actions,
  boardRef,
  choice,
  onMove,
  onChoose,
  onCancel,
}) {
  const attempt =
    view && ["ready", "complete", "abandoned"].includes(view.status);
  return (
    <main className="tempo-core">
      <PageHeader title="Tactics" subtitle="Find the tactic" />
      <Surface>
        <SectionHeader
          title={
            view?.status === "complete" ? "Puzzle complete" : "Your puzzle"
          }
        />
        {view?.prompt && (
          <p>
            {view.prompt.instruction} · {view.prompt.decision_index + 1} /{" "}
            {view.prompt.decision_count}
          </p>
        )}
        {attempt && (
          <>
            <PuzzleBoard
              ref={boardRef}
              view={view}
              locked={
                state.busy ||
                state.canRetry ||
                state.issue === "refresh" ||
                !!choice
              }
              onMove={onMove}
            />
            <PuzzleFeedback view={view} />
          </>
        )}
        {view?.status === "empty" && (
          <StatusNotice>No puzzles are available right now.</StatusNotice>
        )}
        {view?.status === "error" && (
          <StatusNotice kind="error">{view.message}</StatusNotice>
        )}
        {state.issue && state.issue !== "refresh" && (
          <StatusNotice kind="error">{state.issue}</StatusNotice>
        )}
        <PuzzleActions view={view} state={state} actions={actions} />
      </Surface>
      <PromotionChooser
        choice={choice}
        onChoose={onChoose}
        onCancel={onCancel}
      />
    </main>
  );
}
