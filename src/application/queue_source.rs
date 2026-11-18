//! Adapt Queue's target selection to Study's generic question-source port.
//!
//! Catalog and learning snapshots become queue facts. The selected target is
//! then resolved to an exercise. Study never imports Queue, Chess, or Store.
//! The retained block is copied into this adapter and accepted by Tempo only
//! after the issuance transaction succeeds.
use crate::{AppError, app_error::domain};
use std::collections::BTreeSet;
use tempo_chess::ChessDefinition;
use tempo_queue::{Candidate, Group, Line, QueueInput, QueueOrderContext, QueuePolicy};
use tempo_store::Snapshot;
use tempo_study::{Availability, Exercise, QuestionSource};
use tempo_types::{StudyDay, TargetId};

pub(super) struct QueueSource<'a> {
    pub(super) snapshot: &'a Snapshot,
    pub(super) queue: &'a dyn QueuePolicy,
    pub(super) today: StudyDay,
    pub(super) context: QueueOrderContext,
    pub(super) remaining: Vec<TargetId>,
}
impl QuestionSource for QueueSource<'_> {
    type Definition = ChessDefinition;
    type Error = AppError;
    fn next(&mut self) -> Result<Availability<Exercise<ChessDefinition>>, AppError> {
        let candidates = self
            .snapshot
            .catalog
            .items
            .values()
            .map(|item| {
                let learning = &self.snapshot.learning[&item.target];
                Candidate {
                    target: item.target.clone(),
                    active: item.active(),
                    introduced: learning.introduced,
                    due: learning.due,
                    repeat_pending: learning.repeat_pending,
                    independent_new: !item.definition.is_repertoire(),
                }
            })
            .collect();
        let lines = self
            .snapshot
            .catalog
            .lines
            .iter()
            .map(|line| Line {
                id: line.id.clone(),
                targets: line.targets.clone(),
            })
            .collect();
        let eligible = self
            .queue
            .eligible(&QueueInput {
                today: self.today,
                candidates,
                lines,
            })
            .map_err(|e| domain("queue_error", e))?;
        let eligible_ids: BTreeSet<_> = eligible.targets.iter().map(|e| e.target.clone()).collect();
        self.remaining
            .retain(|target| eligible_ids.contains(target));
        let target = if !self.remaining.is_empty() {
            self.remaining.remove(0)
        } else {
            // Assign overlapping line occurrences to their first eligible block once.
            let mut assigned = BTreeSet::new();
            let groups: Vec<_> = self
                .snapshot
                .catalog
                .lines
                .iter()
                .filter_map(|line| {
                    let targets: Vec<_> = line
                        .targets
                        .iter()
                        .filter(|id| eligible_ids.contains(*id) && assigned.insert((*id).clone()))
                        .cloned()
                        .collect();
                    (!targets.is_empty()).then(|| Group {
                        id: line.id.clone(),
                        targets,
                    })
                })
                .collect();
            let blocks = self
                .queue
                .order(&eligible, &groups, &self.context)
                .map_err(|e| domain("queue_error", e))?;
            tempo_queue::validate_grouped_order(&eligible, &groups, &blocks)
                .map_err(|e| domain("queue_error", e))?;
            match tempo_queue::next(&blocks) {
                tempo_queue::Selection::Empty => return Ok(Availability::Empty),
                tempo_queue::Selection::Ready {
                    target,
                    remaining_block,
                } => {
                    self.remaining = remaining_block;
                    target
                }
            }
        };
        let item =
            self.snapshot.catalog.items.get(&target).ok_or_else(|| {
                AppError::new("queue_error", "selection referenced unknown content")
            })?;
        Ok(Availability::Ready(item.exercise()))
    }
}
