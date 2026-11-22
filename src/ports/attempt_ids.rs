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
    use crate::Tempo;
    use tempo_types::AttemptId;

    struct ReservedIds(Vec<AttemptId>);
    impl AttemptIds for ReservedIds {
        fn next_id(&mut self) -> AttemptId {
            self.0.remove(0)
        }
    }

    #[test]
    fn a_custom_id_source_supplies_new_attempts_but_resume_does_not_allocate() {
        let mut components = crate::components::test_components();
        components.ids = Box::new(ReservedIds(vec!["reserved-attempt".into()]));
        let directory = tempfile::tempdir().unwrap();
        let store = crate::components::test_store(
            &directory,
            "ids-demo",
            tempo_chess::demo_catalog(),
            &components,
        );
        let mut app = Tempo::daily(
            Box::new(store),
            components,
            Default::default(),
            [42; 32],
            10,
        )
        .unwrap();
        let issued = app.next_exercise().unwrap().unwrap();
        assert_eq!(issued.attempt.id, AttemptId::from("reserved-attempt"));
        // Only one ID was available. A second Next resumes without asking for one.
        assert_eq!(
            app.next_exercise().unwrap().unwrap().attempt.id,
            issued.attempt.id
        );
    }
}
