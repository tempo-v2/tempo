//! The transport envelope around the supported study intents.
use crate::{AbandonCommand, SubmitCommand};
use serde::{Deserialize, Serialize};
use tempo_types::AttemptId;

/// Deserialize one UI intent, then pass it to [`crate::Tempo::handle`].
///
/// `Next` and `Resume` do not carry grading data. Answer and abandonment wrap
/// typed commands with revision and retry identities. Their payloads reject
/// unknown fields, and unknown command names fail before dispatch. The existing
/// unit `Next` variant ignores extra fields during decoding; those fields are
/// not forwarded to any domain operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum StudyCommand {
    Next,
    Resume { attempt_id: AttemptId },
    Answer(SubmitCommand),
    Acknowledge(SubmitCommand),
    GiveUp(AbandonCommand),
    Skip(AbandonCommand),
    ResumeSkipped,
    Abandon(AbandonCommand),
}

#[cfg(test)]
mod tests {
    use super::StudyCommand;
    use crate::{Tempo, views::StudyView};

    #[test]
    fn a_transport_decodes_an_intent_before_dispatching_it() {
        let command: StudyCommand = serde_json::from_str(r#"{"command":"next"}"#).unwrap();
        let mut app = Tempo::demo().unwrap();
        assert!(matches!(app.handle(command), StudyView::Attempt(_)));
        // A transport cannot dispatch an operation outside the supported intents.
        assert!(
            serde_json::from_str::<StudyCommand>(r#"{"command":"grade","grade":"good"}"#).is_err()
        );
    }
}
