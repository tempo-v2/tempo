//! Explain an issued card using presentation-only facts from its saved version.
use super::{
    Tempo,
    tactic_catalog_facts::{CaptureOrigin, pack_memberships, saved_pgn_source},
    tactic_preferences::tactic_themes,
};
use crate::AppError;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tempo_chess::{ContentItem, TacticalKind};
use tempo_store::{StoredAttempt, StudyScope};
use tempo_study::AttemptStatus;
use tempo_types::{AttemptId, ContentVersion, DatasetId, Grade, Revision};

#[derive(Serialize)]
pub(super) struct CardDetails {
    kind: &'static str,
    sources: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    explanation: Option<CardExplanation>,
}

#[derive(Serialize)]
struct CardExplanation {
    title: String,
    themes: BTreeSet<&'static str>,
    capture_method: BTreeSet<&'static str>,
    game: Option<CardGame>,
    links: BTreeSet<String>,
}

#[derive(Serialize)]
struct CardGame {
    white: String,
    black: String,
    half_move: u32,
    finding: Option<&'static str>,
    url: Option<String>,
}

impl Tempo {
    pub(crate) fn study_card_details(
        &self,
        dataset: &DatasetId,
        id: &AttemptId,
        revision: Revision,
        version: ContentVersion,
    ) -> Result<Value, AppError> {
        let state = self.store.study_state(StudyScope {
            attempt: Some(id),
            ..StudyScope::default()
        })?;
        let Some(record) = state.attempts.get(id).filter(|record| {
            &state.dataset == dataset
                && &record.attempt.dataset == dataset
                && record.attempt.revision == revision
                && record.attempt.content_version == version
        }) else {
            return Err(AppError::new("stale_revision", "This Study card changed."));
        };
        let attempt = &record.attempt;
        let disclosed = attempt.status == AttemptStatus::Complete
            || record.revealed_decision.is_some()
            || attempt.terminal == Some(Grade::Again);
        if !disclosed {
            return Err(AppError::new(
                "card_details_unavailable",
                "Card details are available after a miss or completion.",
            ));
        }
        let active_blind = state
            .active_attempt
            .as_ref()
            .and_then(|id| state.attempts.get(id))
            .is_some_and(|active| active.attempt.target == attempt.target && !active.mode.guided());
        let rich = !active_blind && attempt.status != AttemptStatus::Active;
        let issued = self
            .store
            .issued_item(dataset, &attempt.target, version)?
            .ok_or_else(|| AppError::new("stale_content", "The issued card is unavailable."))?;
        Ok(json!({
            "status": "card_details", "dataset_id": dataset, "attempt_id": id,
            "revision": revision, "content_version": version,
            "details": self.project_card_details(record, &issued, rich)?
        }))
    }

    /// Saved Analysis has already verified disclosure for this exact mark.
    pub(super) fn project_card_details(
        &self,
        record: &StoredAttempt,
        item: &ContentItem,
        rich: bool,
    ) -> Result<CardDetails, AppError> {
        let tactic = !item.definition.is_repertoire();
        let provenance = &item.provenance;
        let legacy_capture = provenance
            .iter()
            .any(|value| value.starts_with("tempo1:capture:"));
        let personal_capture =
            legacy_capture || CaptureOrigin::read(provenance) == CaptureOrigin::PersonalCapture;
        let archived_capture = provenance.iter().any(|value| saved_pgn_source(value));
        let imported_pack = pack_memberships(provenance).next().is_some()
            || provenance
                .iter()
                .any(|value| value.starts_with("tempo1:legacy-deck:"));
        let mut sources = Vec::new();
        if tactic && personal_capture {
            sources.push("Personal capture");
        } else if tactic && archived_capture {
            sources.push("Saved PGN selection");
        }
        if tactic && imported_pack {
            sources.push("Imported puzzle pack");
        }
        let attempt = &record.attempt;
        let game = if tactic {
            self.store.tactic_game_origin(
                &attempt.dataset,
                &attempt.target,
                attempt.content_version,
                record.issued_at.unix_ms,
            )?
        } else {
            None
        };
        if game.is_some() {
            sources.push("Game finding");
        }
        if sources.is_empty() {
            sources.push(if tactic {
                "Source details unavailable"
            } else {
                "Repertoire"
            });
        }
        let explanation = rich.then(|| CardExplanation {
            title: item.label.clone(),
            themes: tactic
                .then(|| {
                    pack_memberships(provenance)
                        .filter_map(|member| {
                            tactic_themes()
                                .iter()
                                .find(|theme| theme.id == member.theme)
                                .map(|theme| theme.name.as_str())
                        })
                        .collect()
                })
                .unwrap_or_default(),
            capture_method: if legacy_capture {
                provenance
                    .iter()
                    .filter_map(|value| match value.strip_prefix("tempo1:source_kind:")? {
                        "manual" => Some("Manual capture"),
                        "game" => Some("Captured from a game"),
                        "puzzle_rush" => Some("Captured from Puzzle Rush"),
                        _ => None,
                    })
                    .collect()
            } else {
                BTreeSet::new()
            },
            game: game.map(|game| CardGame {
                white: game.white,
                black: game.black,
                half_move: game.ply.saturating_add(1),
                finding: [
                    (TacticalKind::HangingMaterial, "Hanging material"),
                    (TacticalKind::WinningMate, "Winning mate"),
                    (TacticalKind::PersonalTactics, "Personal tactics analysis"),
                ]
                .into_iter()
                .find(|(kind, _)| kind.code() == game.motif)
                .map(|(_, label)| label),
                url: matches!(game.source.as_str(), "lichess" | "chess_com")
                    .then_some(game.url)
                    .flatten(),
            }),
            links: if legacy_capture {
                provenance
                    .iter()
                    .filter(|value| value.starts_with("https://"))
                    .cloned()
                    .collect()
            } else {
                BTreeSet::new()
            },
        });
        Ok(CardDetails {
            kind: if tactic { "tactic" } else { "opening" },
            sources,
            explanation,
        })
    }
}
