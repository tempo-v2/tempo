//! Compose today's admissions and durable activity issuance.
use super::{
    TacticThemes, Tempo, tactic_capture::PERSONAL_CAPTURE, tactic_preferences::new_tactic_order,
};
use crate::{
    AppError, Components,
    app_error::domain,
    views::{EmptyView, StudyView},
    wire::STUDY_WIRE_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tempo_chess::ContentItem;
use tempo_learning::RestoredGameObligation;
use tempo_queue::{
    DailyActivity, DailyCandidate, DailyChoice, DailyContext, DailyEvidence, DailyKind,
    DailySelection, DailySession, Line, RelevanceReason, TargetRelevance,
};
use tempo_store::{ActivityMode, AnswerResult, Snapshot, Store, StoredAttempt, StudyScope, Write};
use tempo_study::{Attempt, AttemptStatus};
use tempo_types::{DatasetId, Revision, StudyTime};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IntroductionLimits {
    pub repertoire: usize,
    pub tactics: usize,
}

impl Default for IntroductionLimits {
    fn default() -> Self {
        Self {
            repertoire: 10,
            tactics: 20,
        }
    }
}

pub(super) struct DailyStudy {
    pub(super) limits: IntroductionLimits,
    pub(super) tactic_themes: Option<TacticThemes>,
    pub(super) starter_themes: bool,
    pub(super) queue: DailySession,
    pub(super) preview_key: [u8; 32],
    pub(super) more_tactics: usize,
}

impl Tempo {
    /// Assemble the local release with daily activities and introduction limits.
    pub fn daily(
        store: Box<dyn Store>,
        components: Components,
        limits: IntroductionLimits,
        preview_key: [u8; 32],
        more_tactics: usize,
    ) -> Result<Self, AppError> {
        let mut app = Self::new(store, components)?;
        app.daily = Some(DailyStudy {
            limits,
            tactic_themes: None,
            starter_themes: false,
            queue: DailySession::default(),
            preview_key,
            more_tactics,
        });
        Ok(app)
    }

    pub(super) fn daily_selection(
        &self,
        snapshot: &Snapshot,
        at: StudyTime,
        extra_tactics: usize,
    ) -> Result<DailySelection, AppError> {
        let daily = self.daily.as_ref().expect("daily release");
        let mut admitted = (0, 0);
        let mut evidence = DailyEvidence {
            relevance: self
                .store
                .game_relevance(&snapshot.dataset, tempo_store::REACH_MODEL_KEY)?
                .into_iter()
                .map(|(target, fact)| {
                    (
                        target,
                        TargetRelevance {
                            exposure: fact.exposure,
                            misses: fact.misses,
                            modeled_millionths: fact.modeled_millionths,
                            window_games: fact.window_games,
                        },
                    )
                })
                .collect(),
            ..DailyEvidence::default()
        };
        let mut capture_priority = BTreeMap::new();
        let candidates = snapshot
            .catalog
            .items
            .values()
            .filter_map(|item| {
                let state = &snapshot.learning[&item.target];
                let kind = if item.definition.is_repertoire() {
                    DailyKind::Repertoire
                } else {
                    DailyKind::Tactic
                };
                if state.first_introduced == Some(at.day) {
                    match kind {
                        DailyKind::Repertoire if !state.game_introduction => admitted.0 += 1,
                        DailyKind::Repertoire => {}
                        DailyKind::Tactic if state.taught_at.is_none() => admitted.1 += 1,
                        DailyKind::Tactic => {}
                    }
                }
                if kind == DailyKind::Tactic
                    && state.last_review_at.is_none()
                    && let Some(introduced) = state.taught_at
                    && item
                        .provenance
                        .iter()
                        .any(|value| value == PERSONAL_CAPTURE)
                {
                    capture_priority.insert(item.target.clone(), introduced.unix_ms);
                }
                let may_test = state.can_accept_daily_test(at.day);
                let correction = state
                    .correction
                    .as_ref()
                    .filter(|correction| correction.answer_version == item.version);
                if item.active()
                    && let Some(correction) = correction
                {
                    evidence.corrective_recalls_remaining +=
                        usize::from(correction.remaining_recalls());
                    match correction.next_recall_day {
                        None => {
                            evidence.repair_targets.insert(item.target.clone());
                        }
                        Some(day) if day <= at.day && may_test => {
                            evidence.requested_tests.insert(item.target.clone());
                        }
                        Some(_) => {}
                    }
                } else if item.active() {
                    match state.restored_game_obligation {
                        Some(RestoredGameObligation::Review(review))
                            if may_test && review.actionable_day <= at.day =>
                        {
                            evidence.requested_tests.insert(item.target.clone());
                        }
                        Some(RestoredGameObligation::Teaching) => {
                            evidence.teaching_targets.insert(item.target.clone());
                        }
                        _ => {}
                    }
                }
                item.active().then(|| {
                    (
                        item.target.clone(),
                        DailyCandidate {
                            kind,
                            introduced_this_version: state.introduced
                                && !correction
                                    .is_some_and(|correction| correction.awaiting_introduction),
                            first_introduced: state.first_introduced,
                            due: state.due.filter(|_| may_test && correction.is_none()),
                        },
                    )
                })
            })
            .collect::<BTreeMap<_, _>>();
        let routes = snapshot
            .catalog
            .lines
            .iter()
            .map(|line| Line {
                id: line.id.clone(),
                targets: line.targets.clone(),
            })
            .collect::<Vec<_>>();
        let hash = blake3::hash(snapshot.dataset.0.as_bytes());
        let seed = u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("fixed hash width"));
        let order = new_tactic_order(
            snapshot,
            at.day,
            daily.tactic_themes.as_ref(),
            daily.starter_themes,
        );
        Ok(daily.queue.select_with_evidence(
            DailyContext {
                seed,
                today: at.day,
                repertoire_limit: daily.limits.repertoire,
                tactics_limit: daily.limits.tactics.saturating_add(extra_tactics),
            },
            &candidates,
            &routes,
            admitted,
            snapshot
                .active_attempt
                .as_ref()
                .and_then(|id| snapshot.attempts.get(id))
                .filter(|record| record.mode != ActivityMode::Practice)
                .map(|record| &record.attempt.target),
            order.as_ref(),
            &capture_priority,
            &evidence,
        ))
    }

    pub fn study_relevance_reason(
        &self,
        dataset: &DatasetId,
        attempt: &tempo_types::AttemptId,
    ) -> Result<serde_json::Value, AppError> {
        let reason = self
            .store
            .stored_attempt(attempt)?
            .filter(|record| &record.attempt.dataset == dataset)
            .and_then(|record| record.relevance_reason);
        Ok(serde_json::json!({"reason": reason}))
    }

    pub(super) fn daily_summary(
        &self,
        dataset_id: DatasetId,
        selection: DailySelection,
    ) -> StudyView {
        let more = self.daily.as_ref().expect("daily release").more_tactics;
        let done = selection.outstanding == 0;
        StudyView::Empty(EmptyView {
            schema_version: STUDY_WIRE_VERSION,
            status: "empty".into(),
            dataset_id,
            done,
            corrective_recalls_remaining: (selection.corrective_recalls_remaining > 0)
                .then_some(selection.corrective_recalls_remaining),
            outstanding: selection.outstanding,
            repertoire_remaining: selection.repertoire_remaining,
            tactics_remaining: selection.tactics_remaining,
            can_practice_tactics: done && selection.practice.is_some(),
            can_learn_more_tactics: done && selection.unseen_tactics && more > 0,
            learn_more_count: more,
        })
    }

    pub(super) fn issue_daily(
        &mut self,
        snapshot: &Snapshot,
        at: StudyTime,
        choice: DailyChoice,
    ) -> Result<AnswerResult, AppError> {
        let item = &snapshot.catalog.items[&choice.target];
        let mode = match choice.activity {
            DailyActivity::Teach => ActivityMode::Teach,
            DailyActivity::Repair => ActivityMode::Repair,
            DailyActivity::Test => ActivityMode::Test,
            DailyActivity::Practice => ActivityMode::Practice,
        };
        let start = self
            .components
            .grader
            .start(&item.definition)
            .map_err(|e| domain("invalid_content", e))?;
        let attempt = Attempt {
            id: self.components.ids.next_id(),
            dataset: snapshot.dataset.clone(),
            target: item.target.clone(),
            content_version: item.version,
            revision: Revision(0),
            state: start.state,
            prompt: Some(start.prompt),
            terminal: None,
            status: AttemptStatus::Active,
        };
        let record = StoredAttempt {
            attempt: attempt.clone(),
            issued_at: at,
            issued_miss_revision: snapshot.learning[&item.target].miss_revision,
            mode,
            route: choice.route.clone(),
            first_introduction: choice.new_kind.is_some(),
            exercise_name: exercise_name(item),
            relevance_reason: choice.relevance.map(relevance_reason),
            lesson: match mode {
                ActivityMode::Teach => item.lesson.clone(),
                ActivityMode::Repair => {
                    let context = snapshot.learning[&item.target]
                        .correction
                        .as_ref()
                        .and_then(|correction| correction.previous_move.clone());
                    let expected = &item.definition.decisions[0].expected.uci;
                    let mut lesson =
                        item.lesson
                            .clone()
                            .unwrap_or_else(|| tempo_chess::RepertoireLesson {
                                san: expected.clone(),
                                comments: vec![],
                                previous_move: None,
                            });
                    lesson.previous_move = context;
                    Some(lesson)
                }
                _ => None,
            },
            acceptance: None,
        };
        let scope = StudyScope {
            target: Some(&item.target),
            ..StudyScope::default()
        };
        self.commit_study(
            snapshot,
            vec![
                Write::Issue(record),
                Write::Active(Some(attempt.id.clone())),
            ],
            scope,
        )?;
        self.daily
            .as_mut()
            .expect("daily release")
            .queue
            .issued(&choice, at.day);
        Ok(AnswerResult {
            attempt,
            feedback: None,
            assessment: None,
            learning_effect: None,
        })
    }
}

fn exercise_name(item: &ContentItem) -> Option<String> {
    // Repertoire labels are already public. Tactic catalog labels can reveal
    // motifs, so they are not public display names for blind exercises.
    (item.definition.is_repertoire() && !item.label.trim().is_empty()).then(|| item.label.clone())
}

fn relevance_reason(reason: RelevanceReason) -> String {
    let evidence = reason.evidence;
    if evidence.exposure > 0 {
        let position = if reason.downstream {
            "A later position was reached"
        } else {
            "Reached"
        };
        let games = if evidence.window_games == 1 {
            "game"
        } else {
            "games"
        };
        format!(
            "{position} in {} of your last {} {games}.",
            evidence.exposure, evidence.window_games
        )
    } else {
        let position = if reason.downstream {
            "Modeled reach for a later position"
        } else {
            "Modeled reach"
        };
        format!(
            "{position} · {:.1}% under Maia Rapid 1500.",
            f64::from(evidence.modeled_millionths.unwrap_or(0)) / 10_000.0
        )
    }
}
