import React, { useMemo, useRef, useState } from "react";
import { Board } from "@tempo-v2/core-components";
import { Chess } from "chess.js";

export function play(game, move) {
  return game.move({ from: move.slice(0, 2), to: move.slice(2, 4), promotion: move[4] });
}

export function position(route, ply = route.moves.length) {
  const game = new Chess(route.fen || undefined);
  for (const move of route.moves.slice(0, ply)) play(game, move);
  return game;
}

export function ChessBoard({ game, legal = [], orientation, identity, onMove, onRejectedMove, shapes = [], locked = false }) {
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
  return <div className="board-shell">
    <Board ref={ref} positionIdentity={key} positionRevision={0} fen={fen}
      orientation={orientation} turn={turn} check={game.isCheck() ? turn : false} shapes={shapes}
      mode="legal" readOnly={!onMove} locked={locked || Boolean(choices)}
      retainMoveOnLock={Boolean(choices)} coordinates="edges" maxSize={570}
      destinations={destinations}
      onRejectedMove={onRejectedMove}
      onMove={onMove ? ({ from, to, positionIdentity }) => {
        if (positionIdentity !== key) return;
        const choices = legal.filter((move) => move.startsWith(from + to));
        if (choices.length === 1) onMove(choices[0]);
        if (choices.length > 1) setPromotion({ key, choices });
      } : undefined} />
    {choices && <div className="promotion" role="group" aria-label="Piece to promote to">
      {choices.map((move) => <button key={move} onClick={() => {
        setPromotion(null);
        onMove(move);
      }}>{{ q: "Queen", r: "Rook", b: "Bishop", n: "Knight" }[move[4]]}</button>)}
      <button onClick={() => { setPromotion(null); ref.current?.clearInput(); }}>Cancel</button>
    </div>}
  </div>;
}

export function Replay({ route, color, ply, onPly }) {
  const game = position(route, ply);
  const timeline = position(route).history({ verbose: true });
  return <>
    <ChessBoard game={game} orientation={color} identity="replay" />
    <div className="controls">
      <button disabled={ply === 0} onClick={() => onPly(0)} aria-label="First position">⏮</button>
      <button disabled={ply === 0} onClick={() => onPly(ply - 1)} aria-label="Previous move">◀</button>
      <button disabled={ply === route.moves.length} onClick={() => onPly(ply + 1)} aria-label="Next move">▶</button>
      <button disabled={ply === route.moves.length} onClick={() => onPly(route.moves.length)} aria-label="Last position">⏭</button>
      <span>Position {ply} of {route.moves.length}</span>
    </div>
    <div className="moves" aria-label="Game moves">
      {timeline.map(({ san }, index) => <button className={index + 1 === ply ? "selected" : ""} key={index}
        onClick={() => onPly(index + 1)}>{san}</button>)}
    </div>
  </>;
}
