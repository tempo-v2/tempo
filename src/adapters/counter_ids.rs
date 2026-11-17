//! A process-local attempt ID source for fixtures.
use crate::AttemptIds;
use tempo_types::AttemptId;

/// A process-local attempt ID source for fixtures.
///
/// Use this with fresh in-memory storage. It increments before returning
/// `demo-attempt-N`; restarting from zero can reuse IDs in durable storage.
#[derive(Default)]
pub struct CounterIds(pub u64);
impl AttemptIds for CounterIds {
    fn next_id(&mut self) -> AttemptId {
        self.0 += 1;
        AttemptId(format!("demo-attempt-{}", self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::CounterIds;
    use crate::AttemptIds;

    #[test]
    fn retaining_the_counter_keeps_fixture_attempt_identities_distinct() {
        let mut ids = CounterIds::default();
        let first = ids.next_id();
        let second = ids.next_id();
        assert_ne!(first, second);
        // This explains the adapter's scope: a fresh counter reuses the first ID.
        assert_eq!(CounterIds::default().next_id(), first);
    }
}
