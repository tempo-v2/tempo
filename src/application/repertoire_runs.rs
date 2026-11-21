//! Compose a short run from Chess paths, Learning facts and Queue choices.
use super::Tempo;
use crate::AppError;
use std::collections::BTreeMap;
use tempo_chess::{ContentKind, RepertoireStudyStep, repertoire_study_routes};
use tempo_learning::RepertoireRecall;
use tempo_queue::{DailyChoice, DailySelection};
use tempo_store::{
    ActivityMode, AnswerResult, RepertoireRun, RepertoireRunStep, StudyScope, StudyState, Write,
};
use tempo_study::AttemptStatus;
use tempo_types::{Grade, StudyTime, TargetId};

const MAX_LINE_REVIEWS: usize = 3;

impl Tempo {
    pub(super) fn issue_next_daily(
        &mut self,
        snapshot: &StudyState,
        at: StudyTime,
        selection: &DailySelection,
        connect_repertoires: bool,
    ) -> Result<Option<AnswerResult>, AppError> {
        if let Some(run) = &snapshot.repertoire_run {
            if let Some(choice) = self.continuation(snapshot, at, selection, run)? {
                return self
                    .issue_daily_with_run(snapshot, at, choice, Some(run.clone()))
                    .map(Some);
            }
        }
        let Some(choice) = &selection.next else {
            if snapshot.repertoire_run.is_some() {
                self.commit_study(
                    snapshot,
                    vec![Write::RepertoireRun(None)],
                    StudyScope::default(),
                )?;
            }
            return Ok(None);
        };
        if connect_repertoires
            && snapshot.repertoire_run.is_none()
            && let Some((first_choice, run)) =
                self.plan_repertoire_run(snapshot, at, selection, choice)?
        {
            return self
                .issue_daily_with_run(snapshot, at, first_choice, Some(run))
                .map(Some);
        }
        self.issue_daily(snapshot, at, choice.clone()).map(Some)
    }

    fn recall_fact(
        &self,
        snapshot: &StudyState,
        at: StudyTime,
        target: &TargetId,
    ) -> Result<RepertoireRecall, AppError> {
        let Some((item, state)) = snapshot.target(target)? else {
            return Ok(RepertoireRecall::Standalone);
        };
        let grade = self
            .store
            .latest_applied_test(&snapshot.dataset, target, item.version)?;
        Ok(state
            .daily_availability(item.version, at.day)
            .repertoire_recall(grade, at.day))
    }

    fn plan_repertoire_run(
        &self,
        snapshot: &StudyState,
        at: StudyTime,
        selection: &DailySelection,
        selected: &DailyChoice,
    ) -> Result<Option<(DailyChoice, RepertoireRun)>, AppError> {
        let anchor = &selected.target;
        let Some((item, _)) = snapshot.target(anchor)? else {
            return Ok(None);
        };
        let ContentKind::Repertoire { repertoire, .. } = &item.definition.kind else {
            return Ok(None);
        };
        let (catalog, _) = snapshot.complete_catalog()?;
        if selection.linked_review(anchor).is_none()
            || !catalog.lines.iter().any(|line| {
                &line.repertoire == repertoire
                    && line.targets.len() > 1
                    && line.targets.contains(anchor)
            })
            || self.recall_fact(snapshot, at, anchor)? == RepertoireRecall::Standalone
        {
            return Ok(None);
        }
        let document = self.store.repertoire_document(repertoire)?;
        if document.dataset != snapshot.dataset || document.suspended {
            return Ok(None);
        }
        let (Some(pgn), Some(source_id)) = (document.pgn, document.source_id) else {
            return Ok(None);
        };
        let Ok(routes) = repertoire_study_routes(&pgn, repertoire, catalog.items.values()) else {
            return Ok(None);
        };
        let mut occurrences: Vec<_> = routes
            .iter()
            .flat_map(|route| {
                route.iter().enumerate().filter_map(move |(i, step)| {
                    (&step.target == anchor).then_some((route.as_slice(), i))
                })
            })
            .collect();
        let Some(&(first, index)) = occurrences.first() else {
            return Ok(None);
        };
        let mut first_choice = selected.clone();
        let mut steps = Vec::new();
        let mut anchor_step = run_step(&first[index], ActivityMode::Test);
        anchor_step.lead_in = None; // Use the standalone lead-in when entering here.
        if let Some(parent_index) = index.checked_sub(1) {
            let parent = &first[parent_index];
            let incoming = &first[index].lead_in;
            let unique_parent = incoming.is_some()
                && occurrences.iter().all(|&(route, i)| {
                    i.checked_sub(1).is_some_and(|j| {
                        route[j].target == parent.target
                            && route[j].version == parent.version
                            && &route[i].lead_in == incoming
                    })
                });
            if unique_parent
                && self.recall_fact(snapshot, at, &parent.target)? == RepertoireRecall::Familiar
                && let Some(choice) = self
                    .daily
                    .as_ref()
                    .expect("daily release")
                    .queue
                    .repertoire_warmup(selection, &parent.target)
            {
                let mut warmup = run_step(parent, ActivityMode::Practice);
                warmup.lead_in = None;
                steps.push(warmup);
                anchor_step.lead_in = incoming.clone();
                first_choice = choice;
            }
        }
        steps.push(anchor_step);
        let mut eligibility = BTreeMap::new();
        for _ in 1..MAX_LINE_REVIEWS {
            let mut successor = None;
            for next in occurrences
                .iter()
                .filter_map(|&(route, i)| route.get(i + 1))
            {
                if steps.iter().any(|step| step.target == next.target) {
                    // A variation that returns here sets a run boundary.
                    successor = None;
                    break;
                }
                if next.lead_in.is_none() {
                    continue;
                }
                let eligible = match eligibility.get(&next.target) {
                    Some(&value) => value,
                    None => {
                        let value = selection.linked_review(&next.target).is_some()
                            && self.recall_fact(snapshot, at, &next.target)?
                                != RepertoireRecall::Standalone;
                        eligibility.insert(next.target.clone(), value);
                        value
                    }
                };
                if eligible {
                    if successor.is_some_and(|candidate| candidate != next) {
                        successor = None;
                        break;
                    }
                    successor = Some(next);
                }
            }
            let Some(successor) = successor else {
                break;
            };
            steps.push(run_step(successor, ActivityMode::Test));
            occurrences.retain_mut(|(route, i)| {
                *i += 1;
                route.get(*i) == Some(successor)
            });
        }
        Ok((steps.len() > 1).then_some((
            first_choice,
            RepertoireRun {
                repertoire: repertoire.clone(),
                source_id,
                steps,
                skipped_warmup: false,
            },
        )))
    }

    fn continuation(
        &self,
        snapshot: &StudyState,
        at: StudyTime,
        selection: &DailySelection,
        run: &RepertoireRun,
    ) -> Result<Option<DailyChoice>, AppError> {
        let (Some(next), Some(head)) = (run.next(), run.head()) else {
            return Ok(None);
        };
        let Some(choice) = selection.linked_review(&next.target) else {
            return Ok(None);
        };
        if snapshot.suspended_repertoires.contains(&run.repertoire)
            || self.recall_fact(snapshot, at, &next.target)? == RepertoireRecall::Standalone
        {
            return Ok(None);
        }
        let Some(record) = self.store.stored_attempt(head)? else {
            return Ok(None);
        };
        let completed = match record.mode {
            ActivityMode::Practice => {
                record.attempt.status == AttemptStatus::Complete
                    || (record.attempt.status == AttemptStatus::Abandoned && run.skipped_warmup)
            }
            ActivityMode::Test => {
                record.attempt.status == AttemptStatus::Complete
                    && record.attempt.terminal == Some(Grade::Good)
            }
            _ => false,
        };
        if !completed
            || record.attempt.dataset != snapshot.dataset
            || !current_version(
                snapshot,
                &record.attempt.target,
                record.attempt.content_version,
            )?
            || !current_version(snapshot, &next.target, next.version)?
        {
            return Ok(None);
        }
        let document = self.store.repertoire_document(&run.repertoire)?;
        if document.dataset != snapshot.dataset || document.suspended {
            return Ok(None);
        }
        let (Some(pgn), Some(source_id)) = (document.pgn, document.source_id) else {
            return Ok(None);
        };
        if source_id != run.source_id {
            let (catalog, _) = snapshot.complete_catalog()?;
            let Ok(routes) = repertoire_study_routes(&pgn, &run.repertoire, catalog.items.values())
            else {
                return Ok(None);
            };
            if !routes.iter().any(|route| {
                route.windows(run.steps.len()).any(|path| {
                    path.iter()
                        .zip(&run.steps)
                        .enumerate()
                        .all(|(i, (actual, pinned))| {
                            actual.target == pinned.target
                                && actual.version == pinned.version
                                && (i == 0 || actual.lead_in == pinned.lead_in)
                        })
                })
            }) {
                return Ok(None);
            }
        }
        Ok(Some(choice))
    }
}

fn current_version(
    snapshot: &StudyState,
    target: &TargetId,
    version: tempo_types::ContentVersion,
) -> Result<bool, AppError> {
    Ok(snapshot
        .target(target)?
        .is_some_and(|(item, _)| item.active() && item.version == version))
}

fn run_step(step: &RepertoireStudyStep, mode: ActivityMode) -> RepertoireRunStep {
    RepertoireRunStep {
        target: step.target.clone(),
        version: step.version,
        lead_in: step.lead_in.clone(),
        mode,
        issued: None,
    }
}
