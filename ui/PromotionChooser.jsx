import React from "react";
import { Button, Dialog } from "@tempo-v2/core-components";
export function PromotionChooser({ choice, onChoose, onCancel }) {
  return (
    <Dialog open={!!choice} title="Choose promotion" onClose={onCancel}>
      <div className="actions">
        {["q", "r", "b", "n"]
          .filter((x) => choice?.moves.includes(x))
          .map((x) => (
            <Button key={x} onClick={() => onChoose(choice, x)}>
              {{ q: "Queen", r: "Rook", b: "Bishop", n: "Knight" }[x]}
            </Button>
          ))}
        <Button onClick={onCancel}>Cancel</Button>
      </div>
    </Dialog>
  );
}
