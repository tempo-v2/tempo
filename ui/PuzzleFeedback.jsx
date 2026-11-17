import React from "react";
import { StatusNotice } from "@tempo-v2/core-components";
export function PuzzleFeedback({ view }) {
  return (
    <>
      {view.feedback && (
        <StatusNotice kind={view.feedback.correct ? "info" : "error"}>
          {view.feedback.message}
        </StatusNotice>
      )}
      {view.assessment && (
        <p>Attempt result: {view.assessment === "again" ? "Again" : "Good"}</p>
      )}
    </>
  );
}
