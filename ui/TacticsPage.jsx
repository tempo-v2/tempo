import React from "react";
import { PageHeader, Surface, StatusNotice } from "@tempo-v2/core-components";
import { PuzzleBoard, turnFromFen } from "./PuzzleBoard.jsx";
import { PuzzleFeedback } from "./PuzzleFeedback.jsx";
import { PuzzleActions } from "./PuzzleActions.jsx";
import { PromotionChooser } from "./PromotionChooser.jsx";

export function TacticsPage({
  view,
  state,
  keepMove,
  showSuccess,
  advancing,
  actions,
  boardRef,
  choice,
  onMove,
  onChoose,
  onCancel,
}) {
  const attempt = view && ["ready", "complete", "abandoned"].includes(view.status);
  return (
    <main className={`tempo-core tactics${attempt ? " has-board" : ""}`}>
      {attempt && (
        <div className="tactics-board">
          <PuzzleBoard
            ref={boardRef}
            view={view}
            locked={
              state.busy ||
              state.canRetry ||
              state.issue === "refresh" ||
              !!choice
            }
            retainMoveOnLock={keepMove || !!choice}
            onMove={onMove}
          />
          {showSuccess && (
            <div className="puzzle-success" role="status">
              <span aria-hidden="true">✓</span>
              <span className="visually-hidden">Puzzle solved</span>
            </div>
          )}
        </div>
      )}
      <aside className="tactics-panel" aria-label="Puzzle information">
        <PageHeader title="Tactics" />
        <Surface>
          <div id="puzzle-turn" className="puzzle-turn" role="status">
            {view?.status === "ready" &&
              (turnFromFen(view.display_fen) === "white" ? "White to move" : "Black to move")}
          </div>
          {view?.status === "complete" && !showSuccess && (
            <p>Puzzle complete</p>
          )}
          {view?.status === "abandoned" && <p>Puzzle abandoned</p>}
          {view?.status === "empty" && (
            <StatusNotice>No puzzles are available right now.</StatusNotice>
          )}
          {view?.status === "error" && (
            <StatusNotice kind="error">{view.message}</StatusNotice>
          )}
          <PuzzleFeedback view={view} />
          {state.issue && state.issue !== "refresh" && (
            <StatusNotice kind="error">{state.issue}</StatusNotice>
          )}
          {!showSuccess && !advancing && (
            <PuzzleActions view={view} state={state} actions={actions} />
          )}
        </Surface>
      </aside>
      <PromotionChooser
        choice={choice}
        onChoose={onChoose}
        onCancel={onCancel}
      />
    </main>
  );
}
