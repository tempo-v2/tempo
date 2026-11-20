//! Choose policy and adapter implementations at the application boundary.
use crate::{AttemptIds, ChessGrader, Clock, CounterIds, FixedClock};
use tempo_chess::{ContentParser, ExactSequenceGrader, GameComparator};
use tempo_learning::{FixedIntervalScheduler, LearningPolicy, StandardLearning};
use tempo_queue::{QueuePolicy, StableQueue};
use tempo_types::{StudyDay, StudyTime};

/// Ports for the standalone fixture workflow assembled by [`crate::Tempo::new`].
///
/// Start with [`Self::fixtures`] for a runnable demonstration, then replace a
/// single field with another implementation of the same port. The store is a
/// separate constructor argument because it owns persistence. A game provider is
/// also separate: provider I/O happens before `Tempo::ingest_games`.
pub struct Components {
    /// Eligibility and ordering; produces targets, never attempts.
    pub queue: Box<dyn QueuePolicy>,
    /// Interprets accepted assessments and owns scheduling settings.
    pub learning: Box<dyn LearningPolicy>,
    /// Decides correctness and next prompt for chess exercises.
    pub grader: Box<ChessGrader>,
    /// Converts a standard-profile import request into typed catalog content.
    pub parser: Box<dyn ContentParser>,
    /// Pure comparison of fetched game text against catalog content.
    pub comparator: Box<dyn GameComparator>,
    /// Supplies acceptance and issuance times as explicit values.
    pub clock: Box<dyn Clock>,
    /// Allocates identities for new human attempts.
    pub ids: Box<dyn AttemptIds>,
}
impl Components {
    /// The complete runnable set of fixture policies/adapters, ready for substitution.
    pub fn fixtures() -> Self {
        Self {
            queue: Box::new(StableQueue),
            learning: Box::new(StandardLearning {
                scheduler: FixedIntervalScheduler::default(),
            }),
            grader: Box::new(ExactSequenceGrader),
            parser: Box::new(tempo_chess::demo_parser()),
            comparator: Box::new(tempo_chess::demo_comparator()),
            clock: Box::new(FixedClock(StudyTime {
                day: StudyDay(0),
                unix_ms: 10,
            })),
            ids: Box::new(CounterIds::default()),
        }
    }
}

/// Ports consumed by Daily Study and the signed content and game commands.
pub struct DailyComponents {
    pub learning: Box<dyn LearningPolicy>,
    pub grader: Box<ChessGrader>,
    pub clock: Box<dyn Clock>,
    pub ids: Box<dyn AttemptIds>,
}

impl DailyComponents {
    /// Deterministic baseline ports for disposable integration tests.
    pub fn fixtures() -> Self {
        Components::fixtures().split().0
    }
}

pub(crate) struct FixtureOperations {
    pub(crate) queue: Box<dyn QueuePolicy>,
    pub(crate) parser: Box<dyn ContentParser>,
    pub(crate) comparator: Box<dyn GameComparator>,
}

impl Components {
    pub(crate) fn split(self) -> (DailyComponents, FixtureOperations) {
        (
            DailyComponents {
                learning: self.learning,
                grader: self.grader,
                clock: self.clock,
                ids: self.ids,
            },
            FixtureOperations {
                queue: self.queue,
                parser: self.parser,
                comparator: self.comparator,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Components;
    use crate::Tempo;
    use tempo_queue::ReverseQueue;
    use tempo_store::MemoryStore;
    use tempo_types::TargetId;

    #[test]
    fn selecting_a_different_queue_policy_changes_the_first_exercise() {
        let mut components = Components::fixtures();
        components.queue = Box::new(ReverseQueue);
        let store = MemoryStore::new(
            "policy-demo".into(),
            tempo_chess::demo_catalog(),
            components.learning.settings(),
        )
        .unwrap();
        let mut app = Tempo::new(Box::new(store), components).unwrap();
        assert_eq!(
            app.next_exercise().unwrap().unwrap().attempt.target,
            TargetId::from("t1")
        );
    }
}
