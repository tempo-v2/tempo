//! Bind Study's generic grader port to the concrete chess value types.
use tempo_chess::{ChessAnswer, ChessDefinition, ChessFeedback, ChessPrompt, ChessState};
use tempo_study::Grader;

/// The grader trait object stored in [`crate::Components`].
///
/// Use this alias when injecting a chess grader: it spells out the associated
/// types once. The implementation still decides correctness; Tempo only carries
/// its prompt, feedback and assessment into the surrounding transaction.
pub type ChessGrader = dyn Grader<
        Definition = ChessDefinition,
        State = ChessState,
        Answer = ChessAnswer,
        Prompt = ChessPrompt,
        Feedback = ChessFeedback,
    >;

#[cfg(test)]
mod tests {
    use super::ChessGrader;
    use tempo_chess::{ExactSequenceGrader, demo_catalog};
    use tempo_types::TargetId;

    #[test]
    fn the_alias_accepts_a_chess_grader_behind_studys_generic_interface() {
        let grader: Box<ChessGrader> = Box::new(ExactSequenceGrader);
        let catalog = demo_catalog();
        let definition = &catalog.items[&TargetId::from("t1")].definition;
        let started = grader.start(definition).unwrap();
        // A tactic with two learner decisions still begins with one prompt.
        assert_eq!(started.prompt.decision_index, 0);
        assert_eq!(started.prompt.decision_count, 2);
    }
}
