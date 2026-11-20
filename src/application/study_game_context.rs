//! Read the source of an active guided repair without creating a Study command.

use super::Tempo;
use crate::AppError;
use serde_json::{Value, json};
use tempo_chess::ContentKind;
use tempo_store::{ActivityMode, CorrectionGameLookup, StudyScope};
use tempo_study::AttemptStatus;
use tempo_types::{AttemptId, DatasetId};

impl Tempo {
    pub fn study_game_context(
        &self,
        dataset: &DatasetId,
        attempt_id: &AttemptId,
    ) -> Result<Value, AppError> {
        let _diagnostic =
            crate::diagnostics::Span::new("app.study_game_context.study_game_context");
        let state = self.store.study_state(StudyScope::default())?;
        let unavailable = || Ok(json!({"context": null}));
        if &state.dataset != dataset || state.active_attempt.as_ref() != Some(attempt_id) {
            return unavailable();
        }
        let Some(record) = state.attempts.get(attempt_id).filter(|record| {
            &record.attempt.dataset == dataset
                && record.mode == ActivityMode::Repair
                && record.attempt.status == AttemptStatus::Active
        }) else {
            return unavailable();
        };
        let Some(item) = self.store.issued_item(
            dataset,
            &record.attempt.target,
            record.attempt.content_version,
        )?
        else {
            return unavailable();
        };
        let ContentKind::Repertoire {
            repertoire,
            position,
        } = &item.definition.kind
        else {
            return unavailable();
        };
        let Some(decision) = item.definition.decisions.first() else {
            return unavailable();
        };
        let Some(evidence) = self.store.correction_game(CorrectionGameLookup {
            dataset,
            target: &record.attempt.target,
            version: record.attempt.content_version,
            miss_revision: record.issued_miss_revision,
            issued_at_ms: record.issued_at.unix_ms,
            reset_at_ms: self.store.repertoire_reset_at(dataset, repertoire)?,
            prescribed: &decision.expected.uci,
        })?
        else {
            return unavailable();
        };
        let Ok(Some((played_san, line))) = tempo_chess::played_deviation(
            &evidence.game.pgn,
            evidence.ply,
            position,
            &evidence.played,
        ) else {
            return unavailable();
        };
        // Local PGN uploads have no verified link. Provider import normalizes
        // these URLs when it establishes the raw game identity.
        let url = match evidence.game.source.as_str() {
            "lichess" | "chess_com" => evidence.game.url,
            _ => None,
        };
        Ok(
            json!({"context": {"played_san": played_san, "played_uci": evidence.played, "line": line, "url": url}}),
        )
    }
}
