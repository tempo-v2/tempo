//! Translate one verified capture into Queue inputs. No selection or storage here.

use super::tactic_preferences::TacticThemes;
use chrono_tz::Tz;
use std::collections::{BTreeMap, BTreeSet};
use tempo_chess::ContentStatus;
use tempo_learning::DailyObligation;
use tempo_queue::{
    BatchPlanError, BatchPlanInput, BatchTarget, BatchWork, CapturedIssuedWork, PlanPriority,
    PreparedBatch, TeachAdmission, TeachKind, TeachingRemaining, build_batch_plan,
};
use tempo_store::{
    ActiveStudyWork, StudyContentFacts, StudyContentFamily, StudyPlanInputFacts,
    StudyPlanTargetFacts, StudyThemeSelection,
};
use tempo_types::{StudyDay, TargetId, WorkKind, WorkToken};

#[derive(Debug, PartialEq, Eq)]
enum BuildError {
    UnverifiedCapture,
    LegacyLearning(TargetId),
    InvalidThemes,
    InvalidRoute(String),
    LimitsOutOfRange,
    Queue(BatchPlanError),
}

// The calendar proof comes from the runtime owner. The verified-fixture caller
// exercises this conversion while Store's production capture is Incomplete.
fn build_verified_batch(
    facts: &StudyPlanInputFacts,
    calendar: (Tz, StudyDay),
) -> Result<PreparedBatch, BuildError> {
    let (Some(profile), Some(authority), Some(deferrals)) = (
        facts.profile.as_ref(),
        facts.day_authority.as_ref().filter(|day| day.spend_proven),
        facts.effective_deferrals.as_ref(),
    ) else {
        return Err(BuildError::UnverifiedCapture);
    };
    let basis = &facts.basis.plan;
    if profile.timezone.parse::<Tz>().ok() != Some(calendar.0) || basis.day != calendar.1 {
        return Err(BuildError::UnverifiedCapture);
    }
    let issued_token = match &facts.active_work {
        ActiveStudyWork::None => None,
        ActiveStudyWork::LegacyTokenless { .. } => return Err(BuildError::UnverifiedCapture),
        ActiveStudyWork::Issued {
            issued, correction, ..
        } => Some(correction.as_ref().unwrap_or(issued)),
    };
    let themes = match &profile.tactic_themes {
        StudyThemeSelection::Any {} => TacticThemes::Any,
        StudyThemeSelection::Selected { theme_ids } => TacticThemes::Selected {
            theme_ids: theme_ids.clone(),
        },
    };
    if !themes.valid() {
        return Err(BuildError::InvalidThemes);
    }
    let remaining = TeachingRemaining {
        repertoire: remaining_slots(profile.opening_limit, authority.opening_spent)?,
        tactics: remaining_slots(profile.tactic_limit, authority.tactic_spent)?,
    };
    let frontiers = authored_frontiers(facts)?;
    let targets = facts
        .targets
        .iter()
        .map(|facts| {
            let content = &facts.content;
            let work = learning_work(facts)?;
            let teaching = match &content.family {
                StudyContentFamily::Repertoire { .. } => TeachAdmission::Repertoire {
                    at_frontier: frontiers.contains(&content.target),
                },
                StudyContentFamily::Tactic { membership } => TeachAdmission::Tactic {
                    theme_admitted: facts.availability.first_introduced.is_some()
                        || themes.admits(membership.as_ref()),
                },
            };
            Ok(BatchTarget {
                target: content.target.clone(),
                version: content.version,
                available: available(content),
                teaching,
                work,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let issued = issued_token.map(|token| CapturedIssuedWork {
        work: token.clone(),
        stage_priority: matching_stage_priority(token, &targets),
    });
    build_batch_plan(BatchPlanInput {
        basis,
        expected_targets: facts.expected_targets,
        targets: &targets,
        remaining,
        deferrals,
        issued: issued.as_ref(),
    })
    .map_err(BuildError::Queue)
}

fn remaining_slots(limit: u64, spent: u64) -> Result<usize, BuildError> {
    usize::try_from(limit.saturating_sub(spent)).map_err(|_| BuildError::LimitsOutOfRange)
}

fn available(content: &StudyContentFacts) -> bool {
    content.status == ContentStatus::Active
        && !content.suspended
        && !content.excluded
        && !content.qualification_held
}

fn learning_work(facts: &StudyPlanTargetFacts) -> Result<Option<BatchWork>, BuildError> {
    let availability = &facts.availability;
    let revision = facts.miss_revision;
    match availability.obligation {
        DailyObligation::HumanCorrection => return Ok(Some(BatchWork::Correction { revision })),
        // This older projection masks some routine Learning facts when a
        // legacy corrective ladder is open. It cannot authorize a build.
        DailyObligation::Repair | DailyObligation::CorrectiveRecall { .. } => {
            return Err(BuildError::LegacyLearning(facts.content.target.clone()));
        }
        DailyObligation::None
        | DailyObligation::RestoredTeaching
        | DailyObligation::RestoredReview { .. } => {}
    }
    if availability.introduced_this_version {
        Ok(availability
            .scheduled_due
            .map(|due| BatchWork::ScheduledTest { due, revision }))
    } else {
        let kind = match availability.first_introduced {
            Some(_) => TeachKind::Relearn,
            None => TeachKind::Introduction,
        };
        Ok(Some(BatchWork::Teach { revision, kind }))
    }
}

fn authored_frontiers(facts: &StudyPlanInputFacts) -> Result<BTreeSet<&TargetId>, BuildError> {
    let by_target: BTreeMap<_, _> = facts
        .targets
        .iter()
        .map(|facts| (&facts.content.target, facts))
        .collect();
    let mut frontiers = BTreeSet::new();
    for route in &facts.routes {
        let mut at_frontier = true;
        for target in &route.targets {
            let row = by_target
                .get(target)
                .filter(|row| {
                    matches!(&row.content.family, StudyContentFamily::Repertoire {
                    repertoire_id
                } if repertoire_id == &route.repertoire)
                })
                .ok_or_else(|| BuildError::InvalidRoute(route.id.clone()))?;
            if at_frontier {
                if !available(&row.content) {
                    at_frontier = false;
                } else if !row.availability.introduced_this_version {
                    frontiers.insert(&row.content.target);
                    at_frontier = false;
                }
            }
        }
    }
    Ok(frontiers)
}

fn matching_stage_priority(token: &WorkToken, targets: &[BatchTarget]) -> Option<PlanPriority> {
    let obligation = &token.obligation;
    let target = targets.iter().find(|target| {
        target.available
            && target.target == obligation.target
            && target.version == obligation.version
    })?;
    let (kind, revision, priority) = match target.work? {
        BatchWork::Correction { revision } => {
            (WorkKind::Correction, revision, PlanPriority::Correction)
        }
        BatchWork::ScheduledTest { due, revision } if due <= token.day => {
            (WorkKind::Test, revision, PlanPriority::DueTest(due))
        }
        BatchWork::Teach { revision, .. } => (WorkKind::Teach, revision, PlanPriority::Teach),
        BatchWork::ScheduledTest { .. } => return None,
    };
    (kind == obligation.kind && revision == obligation.revision).then_some(priority)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempo_chess::RepertoireLine;
    use tempo_learning::{
        Correction, CorrectionLifecycle, DailyAction, DailyAvailability, LearningState,
    };
    use tempo_queue::PlanEntryState;
    use tempo_store::{StudyDayAuthority, StudyProfileFacts, StudyProjectionBasis};
    use tempo_types::{
        ContentVersion, MembershipEpoch, MissRevision, PlanBasis, RankRevision, SummaryBasis,
        SummaryEpoch, WorkObligation,
    };

    const TODAY: StudyDay = StudyDay(10);
    const CALENDAR: (Tz, StudyDay) = (chrono_tz::America::New_York, TODAY);

    fn target(id: &str, family: StudyContentFamily) -> StudyPlanTargetFacts {
        StudyPlanTargetFacts {
            content: StudyContentFacts {
                target: id.into(),
                version: ContentVersion(1),
                family,
                status: ContentStatus::Active,
                suspended: false,
                excluded: false,
                qualification_held: false,
            },
            availability: DailyAvailability {
                introduced_this_version: false,
                first_introduced: None,
                scheduled_due: None,
                obligation: DailyObligation::None,
                action: DailyAction::None,
                corrective_recalls_remaining: 0,
            },
            miss_revision: MissRevision(0),
        }
    }

    fn opening(id: &str) -> StudyPlanTargetFacts {
        target(
            id,
            StudyContentFamily::Repertoire {
                repertoire_id: "opening".into(),
            },
        )
    }

    fn tactic(id: &str, theme: Option<&str>) -> StudyPlanTargetFacts {
        target(
            id,
            StudyContentFamily::Tactic {
                membership: theme.map(|theme| {
                    serde_json::from_value(serde_json::json!({
                        "theme_ids": [theme],
                    }))
                    .unwrap()
                }),
            },
        )
    }

    fn route(id: &str, targets: &[&str]) -> RepertoireLine {
        RepertoireLine {
            id: id.into(),
            repertoire: "opening".into(),
            targets: targets.iter().copied().map(TargetId::from).collect(),
        }
    }

    fn capture(
        targets: Vec<StudyPlanTargetFacts>,
        routes: Vec<RepertoireLine>,
    ) -> StudyPlanInputFacts {
        StudyPlanInputFacts {
            basis: StudyProjectionBasis {
                plan: PlanBasis {
                    dataset: "batch".into(),
                    day: TODAY,
                    membership_epoch: MembershipEpoch(3),
                    rank_revision: RankRevision(4),
                },
                summary: SummaryBasis {
                    dataset: "batch".into(),
                    day: TODAY,
                    epoch: SummaryEpoch(2),
                },
                summary_dirty: false,
            },
            profile: Some(StudyProfileFacts {
                timezone: CALENDAR.0.to_string(),
                opening_limit: 2,
                tactic_limit: 1,
                tactic_themes: StudyThemeSelection::Selected {
                    theme_ids: ["fork".into()].into(),
                },
            }),
            day_authority: Some(StudyDayAuthority {
                opening_spent: 1,
                tactic_spent: 1,
                spend_proven: true,
                latest_skip: 0,
                skip_cutoff: 0,
            }),
            effective_deferrals: Some(Vec::new()),
            active_work: ActiveStudyWork::None,
            expected_targets: targets.len(),
            targets,
            routes,
        }
    }

    #[test]
    fn compact_capture_builds_stable_order_with_frontiers_and_independent_allowances() {
        let mut held = opening("held");
        held.content.qualification_held = true;
        let mut prior = opening("prior");
        prior.availability.first_introduced = Some(StudyDay(3));
        prior.availability.introduced_this_version = true;
        let mut relearn = opening("opening-relearn");
        relearn.availability.first_introduced = Some(StudyDay(3));
        let mut tactic_relearn = tactic("tactic-relearn", Some("pin"));
        tactic_relearn.availability.first_introduced = Some(StudyDay(2));
        let mut test = tactic("due-test", None);
        test.availability.introduced_this_version = true;
        test.availability.scheduled_due = Some(StudyDay(8));
        let mut correction = tactic("correction", None);
        correction.availability.obligation = DailyObligation::HumanCorrection;
        correction.miss_revision = MissRevision(2);
        let mut facts = capture(
            vec![
                held,
                opening("independent"),
                prior,
                opening("successor"),
                opening("blocked"),
                opening("opening-over-cap"),
                relearn,
                tactic("new-pin", Some("pin")),
                tactic("new-fork", Some("fork")),
                tactic_relearn,
                test,
                correction,
            ],
            vec![
                route("blocked", &["held", "blocked", "independent"]),
                route("independent", &["prior", "independent"]),
                route("relearn", &["prior", "opening-relearn", "successor"]),
                route("over-cap", &["prior", "opening-over-cap"]),
            ],
        );
        let original = build_verified_batch(&facts, CALENDAR).unwrap();
        facts.targets.reverse();
        facts.routes.reverse();
        assert_eq!(build_verified_batch(&facts, CALENDAR).unwrap(), original);
        let (basis, entries) = original.into_parts();
        assert_eq!(basis, facts.basis.plan);
        assert_eq!(
            entries
                .iter()
                .map(|entry| (
                    entry.work.obligation.target.0.as_str(),
                    entry.priority,
                    entry.state,
                ))
                .collect::<Vec<_>>(),
            [
                (
                    "correction",
                    PlanPriority::Correction,
                    PlanEntryState::Ready
                ),
                (
                    "due-test",
                    PlanPriority::DueTest(StudyDay(8)),
                    PlanEntryState::Ready
                ),
                ("independent", PlanPriority::Teach, PlanEntryState::Ready),
                (
                    "opening-relearn",
                    PlanPriority::Teach,
                    PlanEntryState::Ready
                ),
                ("tactic-relearn", PlanPriority::Teach, PlanEntryState::Ready),
            ],
        );
        facts.day_authority.as_mut().unwrap().tactic_spent = 0;
        let (_, entries) = build_verified_batch(&facts, CALENDAR).unwrap().into_parts();
        assert!(
            entries
                .iter()
                .any(|entry| entry.work.obligation.target.0 == "new-fork")
        );
        assert!(
            !entries
                .iter()
                .any(|entry| entry.work.obligation.target.0 == "new-pin")
        );
    }

    #[test]
    fn active_correction_uses_the_immutable_authority_token_and_exact_deferral() {
        let mut correction = tactic("tactic", Some("fork"));
        correction.availability.obligation = DailyObligation::HumanCorrection;
        correction.miss_revision = MissRevision(2);
        let mut facts = capture(vec![correction], Vec::new());
        let token = |kind, day, revision| WorkToken {
            day: StudyDay(day),
            obligation: WorkObligation {
                target: "tactic".into(),
                version: ContentVersion(1),
                kind,
                revision: MissRevision(revision),
            },
        };
        let correction = token(WorkKind::Correction, 10, 2);
        facts.active_work = ActiveStudyWork::Issued {
            attempt_id: "existing-test".into(),
            issued: token(WorkKind::Test, 9, 1),
            correction: Some(correction.clone()),
        };
        facts.effective_deferrals =
            Some(vec![token(WorkKind::Correction, 9, 1).deferred_on(TODAY)]);
        let (_, entries) = build_verified_batch(&facts, CALENDAR).unwrap().into_parts();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].work, correction);
        assert_eq!(entries[0].state, PlanEntryState::Issued);

        facts.active_work = ActiveStudyWork::None;
        facts
            .effective_deferrals
            .as_mut()
            .unwrap()
            .push(correction.deferred_on(TODAY));
        let (_, entries) = build_verified_batch(&facts, CALENDAR).unwrap().into_parts();
        assert_eq!(entries[0].state, PlanEntryState::Deferred);
    }

    #[test]
    fn legacy_correction_cannot_replace_the_prepared_routine_due_day() {
        let mut routine = opening("routine");
        let mut learning = LearningState {
            introduced: true,
            first_introduced: Some(StudyDay(2)),
            due: Some(StudyDay(8)),
            correction: Some(Correction {
                lifecycle: CorrectionLifecycle::LegacyGame,
                answer_version: routine.content.version,
                clean_recalls: 0,
                awaiting_introduction: false,
                next_recall_day: Some(StudyDay(3)),
                previous_move: None,
            }),
            ..LearningState::default()
        };
        routine.availability = learning.prepared_availability(routine.content.version, TODAY);
        let mut facts = capture(vec![routine], vec![route("route", &["routine"])]);
        let (_, entries) = build_verified_batch(&facts, CALENDAR).unwrap().into_parts();
        assert_eq!(entries[0].priority, PlanPriority::DueTest(StudyDay(8)));

        learning.due = Some(StudyDay(11));
        let routine = &mut facts.targets[0];
        routine.availability = learning.prepared_availability(routine.content.version, TODAY);
        let (_, entries) = build_verified_batch(&facts, CALENDAR).unwrap().into_parts();
        assert!(entries.is_empty());
    }

    #[test]
    fn refuses_missing_authority_and_masked_legacy_learning() {
        let mut facts = capture(vec![tactic("tactic", None)], Vec::new());
        assert_eq!(
            build_verified_batch(&facts, (CALENDAR.0, StudyDay(11))),
            Err(BuildError::UnverifiedCapture),
        );
        facts.active_work = ActiveStudyWork::LegacyTokenless {
            attempt_id: "older".into(),
        };
        assert_eq!(
            build_verified_batch(&facts, CALENDAR),
            Err(BuildError::UnverifiedCapture)
        );
        facts.active_work = ActiveStudyWork::None;
        facts.targets[0].availability.obligation = DailyObligation::CorrectiveRecall { day: TODAY };
        assert_eq!(
            build_verified_batch(&facts, CALENDAR),
            Err(BuildError::LegacyLearning("tactic".into()))
        );
    }
}
