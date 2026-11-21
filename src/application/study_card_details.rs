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
use tempo_store::{
    IssuedTacticAttribution, StoredAttempt, StudyScope, TacticGameLookup, TacticGameRelation,
};
use tempo_study::AttemptStatus;
use tempo_types::{AttemptId, ContentVersion, DatasetId, Grade, Revision};

enum CardDisclosure {
    SourceOnly,
    Full,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum CardKind {
    Opening,
    Tactic,
}

#[derive(Serialize)]
pub(super) struct CardDetails {
    kind: CardKind,
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
    /// Capture current presentation facts on the same revision-checked issuance
    /// as the attempt. A later import or rescan cannot rewrite this attribution.
    pub(super) fn issued_tactic_attribution(
        &self,
        dataset: &DatasetId,
        item: &ContentItem,
        request: Option<u64>,
    ) -> Result<Option<IssuedTacticAttribution>, AppError> {
        if item.definition.is_repertoire() {
            return Ok(None);
        }
        let lookup = request.map_or(TacticGameLookup::Related, TacticGameLookup::Request);
        let game = self
            .store
            .tactic_game_origin(dataset, &item.target, item.version, lookup)?;
        Ok(Some(IssuedTacticAttribution {
            title: item.label.clone(),
            provenance: item.provenance.clone(),
            game,
            game_capture: item.game_capture.clone(),
        }))
    }

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
        let disclosure = if !active_blind && attempt.status != AttemptStatus::Active {
            CardDisclosure::Full
        } else {
            CardDisclosure::SourceOnly
        };
        let issued = self
            .store
            .issued_item(dataset, &attempt.target, version)?
            .ok_or_else(|| AppError::new("stale_content", "The issued card is unavailable."))?;
        Ok(json!({
            "status": "card_details", "dataset_id": dataset, "attempt_id": id,
            "revision": revision, "content_version": version,
            "details": self.project_card_details(record, &issued, disclosure)?
        }))
    }

    /// The saved-mark read has already checked the exact decision's disclosure.
    pub(super) fn project_saved_card_details(
        &self,
        record: &StoredAttempt,
    ) -> Result<CardDetails, AppError> {
        let attempt = &record.attempt;
        let issued = self
            .store
            .issued_item(&attempt.dataset, &attempt.target, attempt.content_version)?
            .ok_or_else(|| AppError::new("stale_content", "The issued card is unavailable."))?;
        let disclosure = if attempt.status == AttemptStatus::Active {
            CardDisclosure::SourceOnly
        } else {
            CardDisclosure::Full
        };
        self.project_card_details(record, &issued, disclosure)
    }

    fn project_card_details(
        &self,
        record: &StoredAttempt,
        item: &ContentItem,
        disclosure: CardDisclosure,
    ) -> Result<CardDetails, AppError> {
        let tactic = !item.definition.is_repertoire();
        let attribution = record.tactic_attribution.as_ref();
        let provenance = attribution.map_or(item.provenance.as_slice(), |facts| &facts.provenance);
        let title = if tactic {
            attribution.map_or(item.label.as_str(), |facts| &facts.title)
        } else {
            record.exercise_name.as_deref().unwrap_or(&item.label)
        };
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
        let game = match attribution {
            Some(facts) => facts.game.clone(),
            None if tactic => self.store.tactic_game_origin(
                &attempt.dataset,
                &attempt.target,
                attempt.content_version,
                TacticGameLookup::LegacyAttempt(&attempt.id),
            )?,
            None => None,
        };
        if let Some(game) = &game {
            sources.push(match game.relation {
                TacticGameRelation::IssuingFinding => "Game finding",
                TacticGameRelation::RelatedFinding => "Related game finding",
            });
        }
        if sources.is_empty() {
            sources.push(if tactic {
                "Source details unavailable"
            } else {
                "Repertoire"
            });
        }
        let explanation = matches!(disclosure, CardDisclosure::Full).then(|| CardExplanation {
            title: title.to_owned(),
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
            kind: if tactic {
                CardKind::Tactic
            } else {
                CardKind::Opening
            },
            sources,
            explanation,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SubmitCommand, local_runtime};
    use tempo_chess::{START_FEN, TacticRecord, compile_tactics};
    use tempo_store::{Commit, Write};

    #[test]
    fn describe_capture_added_before_issue_and_pin_attribution_through_later_edits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("card-origin.sqlite");
        local_runtime::init(&path).unwrap();
        let mut app = local_runtime::open(&path).unwrap();
        let catalog = compile_tactics(&[TacticRecord {
            namespace: "origin-test".into(),
            source_id: "opening".into(),
            solver_fen: START_FEN.into(),
            solution: ["e2e4", "e7e5", "g1f3"].map(str::to_owned).to_vec(),
            label: "Original catalog label".into(),
            provenance: vec![],
        }])
        .unwrap();
        let target = catalog.items.keys().next().unwrap().clone();
        let state = app.snapshot().unwrap();
        let dataset = state.dataset;
        app.store
            .commit(Commit {
                expected_dataset: dataset.clone(),
                expected_revision: state.revision,
                writes: vec![
                    Write::Catalog(catalog),
                    Write::Learning {
                        target: target.clone(),
                        state: Default::default(),
                    },
                ],
            })
            .unwrap();
        let pgn = format!(
            "[Event \"Same puzzle\"]\n[SetUp \"1\"]\n[FEN \"{START_FEN}\"]\n[TempoSetupPlies \"0\"]\n\n1. e4 e5 2. Nf3 *"
        );
        let preview = app.handle_tactic_capture(
            serde_json::from_value(json!({
                "command": "preview", "dataset_id": dataset, "pgn": pgn,
            }))
            .unwrap(),
        );
        let saved = app.handle_tactic_capture(
            serde_json::from_value(json!({
                "command": "save", "dataset_id": dataset, "pgn": pgn,
                "token": preview["token"], "selected": [0],
            }))
            .unwrap(),
        );
        assert_eq!(saved["ready_for_review"], 1);
        let issued = app.next_exercise().unwrap().unwrap().attempt;
        assert_eq!(issued.target, target);
        assert_eq!(issued.content_version, ContentVersion(1));
        let snapshot = app.snapshot().unwrap();
        let mut catalog = snapshot.catalog;
        let item = catalog.items.get_mut(&target).unwrap();
        let issued_title = item.label.clone();
        item.label = "New title after issuance".into();
        app.store
            .commit(Commit {
                expected_dataset: dataset.clone(),
                expected_revision: snapshot.revision,
                writes: vec![Write::Catalog(catalog)],
            })
            .unwrap();
        let mut attempt = issued;
        for (index, answer) in ["e2e4", "g1f3"].into_iter().enumerate() {
            attempt = app
                .submit(SubmitCommand {
                    dataset_id: dataset.clone(),
                    attempt_id: attempt.id,
                    expected_revision: attempt.revision,
                    submission_id: tempo_types::SubmissionId(format!("attribution-answer-{index}")),
                    answer: answer.into(),
                })
                .unwrap()
                .attempt;
        }
        let reply = app
            .study_card_details(
                &dataset,
                &attempt.id,
                attempt.revision,
                attempt.content_version,
            )
            .unwrap();
        assert_eq!(reply["details"]["sources"], json!(["Personal capture"]));
        assert_eq!(reply["details"]["explanation"]["title"], issued_title);
    }
}
