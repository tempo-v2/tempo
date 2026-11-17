//! A clock that always returns the supplied logical day and timestamp.
use crate::Clock;
use tempo_types::StudyTime;

/// A clock that always returns the supplied logical day and timestamp.
///
/// Use this in demonstrations or tests where the due day and acceptance time
/// must be explicit. A production clock implements [`crate::Clock`] separately.
pub struct FixedClock(pub StudyTime);
impl Clock for FixedClock {
    fn now(&self) -> StudyTime {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::FixedClock;
    use crate::Clock;
    use tempo_types::{StudyDay, StudyTime};

    #[test]
    fn repeated_reads_keep_a_boundary_day_scenario_deterministic() {
        let clock = FixedClock(StudyTime {
            day: StudyDay(7),
            unix_ms: 86_399_999,
        });
        let issued = clock.now();
        let accepted = clock.now();
        assert_eq!(issued, accepted);
        // A scenario can cross the study-day boundary explicitly, without sleeping.
        let tomorrow = FixedClock(StudyTime {
            day: StudyDay(8),
            unix_ms: 86_400_000,
        });
        assert!(tomorrow.now().day > accepted.day);
    }
}
