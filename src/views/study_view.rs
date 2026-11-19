//! The three reply shapes a presentation client must handle.
use super::attempt_view::AttemptFeedback;
use crate::views::{AttemptView, EmptyView, ErrorView, LessonView};
use crate::{AppError, wire::STUDY_WIRE_VERSION};
use serde::{Deserialize, Serialize};
use tempo_chess::PositionDisplay;
use tempo_store::{AnswerResult, StoredAttempt};
use tempo_study::AttemptStatus;

/// A versioned reply carrying a displayable attempt, an empty queue, or an error.
///
/// [`crate::Tempo::handle`] produces this; transports serialize it and clients
/// branch on `status`. The projection carries the attempt's
/// lifetime terminal grade, even when a corrective move emits no *new* assessment.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StudyView {
    Attempt(Box<AttemptView>),
    Empty(EmptyView),
    Error(ErrorView),
}

impl From<AppError> for StudyView {
    fn from(error: AppError) -> Self {
        Self::Error(ErrorView::new(error, STUDY_WIRE_VERSION))
    }
}
impl StudyView {
    /// Keep the original strict v5 shape for consumers that did not opt in
    /// to supplemental feedback labels. The attempt and receipt stay fixed.
    pub fn without_move_details(mut self) -> Self {
        if let Self::Attempt(view) = &mut self
            && let Some(feedback) = &mut view.feedback
        {
            feedback.correction_san = None;
        }
        self
    }

    pub(crate) fn attempt(
        value: AnswerResult,
        board: PositionDisplay,
        correction: Option<(String, String)>,
        record: &StoredAttempt,
        lesson: Option<LessonView>,
        history: Option<tempo_chess::RevealedLine>,
    ) -> Self {
        let attempt = value.attempt;
        let status = match attempt.status {
            AttemptStatus::Active => "ready",
            AttemptStatus::Complete => "complete",
            AttemptStatus::Abandoned => "abandoned",
        };
        let (correction_uci, correction_san) = correction
            .map(|(uci, san)| (Some(uci), Some(san)))
            .unwrap_or_default();
        Self::Attempt(Box::new(AttemptView {
            schema_version: STUDY_WIRE_VERSION,
            status: status.into(),
            activity: if record.mode == tempo_store::ActivityMode::Repair {
                tempo_store::ActivityMode::Teach
            } else {
                record.mode
            },
            corrective_repair: (record.mode == tempo_store::ActivityMode::Repair).then_some(true),
            lesson,
            exercise_name: record.exercise_name.clone(),
            history,
            dataset_id: attempt.dataset,
            attempt_id: attempt.id,
            target_id: attempt.target,
            content_version: attempt.content_version,
            revision: attempt.revision,
            display_fen: board.display_fen,
            learner_color: board.learner_color,
            legal_moves: board.legal_moves,
            in_check: board.in_check,
            prompt: attempt.prompt,
            feedback: value.feedback.map(|result| AttemptFeedback {
                result,
                correction_uci,
                correction_san,
            }),
            assessment: attempt.terminal,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::StudyView;
    use crate::{SubmitCommand, Tempo, wire::StudyCommand};
    use tempo_types::Grade;

    #[test]
    fn a_resumed_correction_still_displays_the_attempts_original_again_grade() {
        let mut app = Tempo::demo().unwrap();
        let issued = app.next_exercise().unwrap().unwrap();
        app.submit(SubmitCommand {
            dataset_id: issued.attempt.dataset,
            attempt_id: issued.attempt.id.clone(),
            submission_id: "mistake".into(),
            expected_revision: issued.attempt.revision,
            answer: "e2e3".into(),
        })
        .unwrap();
        let resumed = app.resume(&issued.attempt.id).unwrap();
        assert_eq!(resumed.assessment, None); // no new assessment from a read
        let StudyView::Attempt(view) = app.handle(StudyCommand::Resume {
            attempt_id: issued.attempt.id,
        }) else {
            panic!("attempt view")
        };
        assert_eq!(view.status, "ready");
        assert_eq!(view.assessment, Some(Grade::Again));
    }
}
