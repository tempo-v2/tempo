//! Allocate an identity when Tempo issues a fresh attempt.
use tempo_types::AttemptId;

/// Allocate an identity when Tempo issues a fresh attempt.
///
/// Implement this at composition time if storage survives process restarts.
/// Study receives the resulting ID as data and never chooses an ID strategy.
/// The fixture [`crate::CounterIds`] is only unique within its retained counter.
pub trait AttemptIds {
    fn next_id(&mut self) -> AttemptId;
}

#[cfg(test)]
mod tests {
    use super::AttemptIds;
    use crate::{Components, Tempo};
    use tempo_store::MemoryStore;
    use tempo_types::AttemptId;

    struct ReservedIds(Vec<AttemptId>);
    impl AttemptIds for ReservedIds {
        fn next_id(&mut self) -> AttemptId {
            self.0.remove(0)
        }
    }

    #[test]
    fn a_custom_id_source_supplies_new_attempts_but_resume_does_not_allocate() {
        let mut components = Components::fixtures();
        components.ids = Box::new(ReservedIds(vec!["reserved-attempt".into()]));
        let store = MemoryStore::new(
            "ids-demo".into(),
            tempo_chess::demo_catalog(),
            components.learning.settings(),
        )
        .unwrap();
        let mut app = Tempo::new(Box::new(store), components).unwrap();
        let issued = app.next_exercise().unwrap().unwrap();
        assert_eq!(issued.attempt.id, AttemptId::from("reserved-attempt"));
        // Only one ID was available. A second Next resumes without asking for one.
        assert_eq!(
            app.next_exercise().unwrap().unwrap().attempt.id,
            issued.attempt.id
        );
    }
}
