import React, { useLayoutEffect, useMemo, useRef, useState } from "react";
import { Board } from "@tempo-v2/core-components";
import { createPortal } from "react-dom";
import { Chess } from "chess.js";
import { moveCue } from "./sounds.js";

const promotions = {
  q: { role: "queen", label: "Queen" },
  r: { role: "rook", label: "Rook" },
  b: { role: "bishop", label: "Bishop" },
  n: { role: "knight", label: "Knight" },
};

export function play(game, move) {
  return game.move({ from: move.slice(0, 2), to: move.slice(2, 4), promotion: move[4] });
}

export function position(route, ply = route.moves.length) {
  const game = new Chess(route.fen || undefined);
  for (const move of route.moves.slice(0, ply)) play(game, move);
  return game;
}

export function ChessBoard({ game, legal = [], orientation, identity, onMove, onRejectedMove, lastMove, shapes, promotionTarget, locked = false, retainMoveOnLock = false }) {
  const [promotion, setPromotion] = useState(null);
  const ref = useRef(null);
  const fen = game.fen();
  const destinations = useMemo(() => {
    const result = new Map();
    for (const move of legal) {
      const from = move.slice(0, 2);
      const to = move.slice(2, 4);
      if (!result.has(from)) result.set(from, []);
      if (!result.get(from).includes(to)) result.get(from).push(to);
    }
    return result;
  }, [legal]);
  const key = identity + fen;
  const choices = promotion?.key === key ? promotion.choices : null;
  const turn = game.turn() === "w" ? "white" : "black";
  const promotionControls = choices && <div className="promotion" role="group" aria-label="Piece to promote to">
    {choices.map((move) => {
      const { role, label } = promotions[move[4]];
      return <button key={move} onClick={() => {
        ref.current?.previewPromotion(move.slice(2, 4), role);
        setPromotion(null);
        onMove(move);
      }}>{label}</button>;
    })}
    <button onClick={() => { setPromotion(null); ref.current?.clearInput(); }}>Cancel</button>
  </div>;
  return <div className="board-shell">
    <Board ref={ref} positionIdentity={key} positionRevision={0} fen={fen}
      orientation={orientation} turn={turn} check={game.isCheck() ? turn : false} lastMove={lastMove} shapes={shapes}
      mode="legal" readOnly={!onMove} locked={locked || Boolean(choices)}
      retainMoveOnLock={Boolean(choices) || retainMoveOnLock} coordinates="edges" maxSize={570}
      destinations={destinations}
      onRejectedMove={onRejectedMove}
      onMove={onMove ? ({ from, to, positionIdentity }) => {
        if (positionIdentity !== key) return;
        const choices = legal.filter((move) => move.startsWith(from + to));
        if (choices.length === 1) onMove(choices[0]);
        if (choices.length > 1) setPromotion({ key, choices });
      } : undefined} />
    {promotionControls && (promotionTarget ? createPortal(promotionControls, promotionTarget) : promotionControls)}
  </div>;
}

export function Replay({ route, color, ply, onPly, sounds, controlsTarget }) {
  useLayoutEffect(() => sounds.cancel, [sounds, route, color]);
  const timeline = useMemo(() => position(route).history({ verbose: true }), [route]);
  const game = new Chess(timeline[Math.min(ply, timeline.length) - 1]?.after || route.fen || undefined);
  function seek(next) {
    if (next > ply) {
      const move = timeline[next - 1];
      const board = new Chess(move.after);
      const player = color === "white" ? "w" : "b";
      sounds.play(moveCue(move, board, move.color === player ? "move-self" : "move-opponent"));
    } else if (next < ply) sounds.cancel();
    onPly(next);
  }
  const board = <ChessBoard game={game} orientation={color} identity="replay" />;
  const controls = <>
    <div className="controls">
      <button disabled={ply === 0} onClick={() => seek(0)} aria-label="First position">⏮</button>
      <button disabled={ply === 0} onClick={() => seek(ply - 1)} aria-label="Previous move">◀</button>
      <button disabled={ply === route.moves.length} onClick={() => seek(ply + 1)} aria-label="Next move">▶</button>
      <button disabled={ply === route.moves.length} onClick={() => seek(route.moves.length)} aria-label="Last position">⏭</button>
      <span>Position {ply} of {route.moves.length}</span>
    </div>
    <div className="moves" aria-label="Game moves">
      {timeline.map(({ san }, index) => <button className={index + 1 === ply ? "selected" : ""} key={index}
        onClick={() => seek(index + 1)}>{san}</button>)}
    </div>
  </>;
  return <>{board}{controlsTarget ? createPortal(controls, controlsTarget) : controls}</>;
}
