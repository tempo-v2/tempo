//! Read the source of an active guided repair without creating a Study command.

use super::Tempo;
use crate::AppError;
use serde_json::{Value, json};
use tempo_chess::{ContentItem, ContentKind, OpponentLeadIn};
use tempo_store::{ActivityMode, CorrectionGame, CorrectionGameLookup, StoredAttempt, StudyScope};
use tempo_study::AttemptStatus;
use tempo_types::{AttemptId, DatasetId};

impl Tempo {
    pub fn study_game_context(
        &self,
        dataset: &DatasetId,
        attempt_id: &AttemptId,
    ) -> Result<Value, AppError> {
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
        let ContentKind::Repertoire { position, .. } = &item.definition.kind else {
            return unavailable();
        };
        let Some(evidence) = self.issued_repair_game(record, &item)? else {
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

    pub(super) fn repair_lead_in(
        &self,
        record: &StoredAttempt,
        item: &ContentItem,
    ) -> Option<OpponentLeadIn> {
        let ContentKind::Repertoire { position, .. } = &item.definition.kind else {
            return None;
        };
        let evidence = self.issued_repair_game(record, item).ok()??;
        tempo_chess::played_deviation_lead_in(
            &evidence.game.pgn,
            evidence.ply,
            position,
            &evidence.played,
        )
        .ok()
        .flatten()
    }

    fn issued_repair_game(
        &self,
        record: &StoredAttempt,
        item: &ContentItem,
    ) -> Result<Option<CorrectionGame>, AppError> {
        let ContentKind::Repertoire { repertoire, .. } = &item.definition.kind else {
            return Ok(None);
        };
        let Some(decision) = item.definition.decisions.first() else {
            return Ok(None);
        };
        let attempt = &record.attempt;
        Ok(self.store.correction_game(CorrectionGameLookup {
            dataset: &attempt.dataset,
            target: &attempt.target,
            version: attempt.content_version,
            miss_revision: record.issued_miss_revision,
            issued_at_ms: record.issued_at.unix_ms,
            reset_at_ms: self
                .store
                .repertoire_reset_at(&attempt.dataset, repertoire)?,
            prescribed: &decision.expected.uci,
        })?)
    }
}
