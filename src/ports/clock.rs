//! Read application time without teaching domain policies about the system clock.
use tempo_types::StudyTime;

/// Read application time without teaching domain policies about the system clock.
///
/// The composition root supplies this port. Tempo reads it when issuing attempts
/// and accepting assessments, then passes plain `StudyTime` values downstream.
/// Implement it for real time; use [`crate::FixedClock`] for repeatable examples.
pub trait Clock {
    fn now(&self) -> StudyTime;
    fn day_at(&self, unix_ms: u64) -> tempo_types::StudyDay {
        let now = self.now();
        let offset = (unix_ms / 86_400_000) as i64 - (now.unix_ms / 86_400_000) as i64;
        tempo_types::StudyDay((i64::from(now.day.0) + offset) as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::Clock;
    use crate::Tempo;
    use tempo_types::{StudyDay, StudyTime};

    struct ScenarioClock;
    impl Clock for ScenarioClock {
        fn now(&self) -> StudyTime {
            StudyTime {
                day: StudyDay(12),
                unix_ms: 1_000,
            }
        }
    }

    #[test]
    fn an_injected_clock_supplies_the_time_saved_with_an_issued_attempt() {
        let mut components = crate::components::test_components();
        components.clock = Box::new(ScenarioClock);
        let directory = tempfile::tempdir().unwrap();
        let store = crate::components::test_store(
            &directory,
            "clock-demo",
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
        let snapshot = app.snapshot().unwrap();
        assert_eq!(
            snapshot.attempts[&issued.attempt.id].issued_at,
            ScenarioClock.now()
        );
    }
}
