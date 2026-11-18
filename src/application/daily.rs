//! Compose today's admissions and durable activity issuance.
use super::Tempo;
use crate::{AppError, Components, app_error::domain};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tempo_chess::ContentItem;
use tempo_queue::{
    DailyActivity, DailyCandidate, DailyChoice, DailyContext, DailyKind, DailySelection,
    DailySession, Line,
};
use tempo_store::{ActivityMode, AnswerResult, Snapshot, Store, StoredAttempt, StudyScope, Write};
use tempo_study::{Attempt, AttemptStatus};
use tempo_types::{Revision, StudyTime};

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
    pub(super) queue: DailySession,
    pub(super) preview_key: [u8; 32],
}

impl Tempo {
    /// Assemble the local release with daily activities and introduction limits.
    pub fn daily(
        store: Box<dyn Store>,
        components: Components,
        limits: IntroductionLimits,
        preview_key: [u8; 32],
    ) -> Result<Self, AppError> {
        let mut app = Self::new(store, components)?;
        app.daily = Some(DailyStudy {
            limits,
            queue: DailySession::default(),
            preview_key,
        });
        Ok(app)
    }

    pub(super) fn daily_selection(&self, snapshot: &Snapshot, at: StudyTime) -> DailySelection {
        let daily = self.daily.as_ref().expect("daily release");
        let mut admitted = (0, 0);
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
                        DailyKind::Repertoire => admitted.0 += 1,
                        DailyKind::Tactic => admitted.1 += 1,
                    }
                }
                item.active().then(|| {
                    (
                        item.target.clone(),
                        DailyCandidate {
                            kind,
                            introduced_this_version: state.introduced,
                            first_introduced: state.first_introduced,
                            due: state.due,
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
        daily.queue.select(
            DailyContext {
                seed,
                today: at.day,
                repertoire_limit: daily.limits.repertoire,
                tactics_limit: daily.limits.tactics,
            },
            &candidates,
            &routes,
            admitted,
        )
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
            repertoire_name: repertoire_name(item),
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

fn repertoire_name(item: &ContentItem) -> Option<String> {
    item.definition.is_repertoire().then(|| item.label.clone())
}
