//! Transient Study presence. The HTTP client reports focus, while the
//! runtime owns timing and uses its monotonic clock.
use serde::Deserialize;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

const QUIET_PERIOD: Duration = Duration::from_secs(30);
const RETAIN_ORDER: Duration = Duration::from_secs(120);
const MAX_TABS: usize = 256;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PresenceReport {
    pub(super) tab: String,
    pub(super) sequence: u64,
    pub(super) state: PresenceState,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum PresenceState {
    Active,
    Inactive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StudyWait {
    Active,
    QuietPeriod,
}

struct Tab {
    sequence: u64,
    state: PresenceState,
    reported: Instant,
    blocked_until: Instant,
}

pub(super) struct StudyPresence {
    tabs: HashMap<String, Tab>,
    command_until: Instant,
}

impl StudyPresence {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            tabs: HashMap::new(),
            command_until: now + QUIET_PERIOD,
        }
    }

    /// Returns whether an accepted update adds a blocker that may interrupt analysis.
    /// An unseen release may have overtaken its activation in the network.
    /// Repeated releases from a known inactive tab leave its deadline alone.
    pub(super) fn report(
        &mut self,
        report: PresenceReport,
        now: Instant,
    ) -> Result<bool, &'static str> {
        if report.tab.len() != 36
            || !report.tab.bytes().enumerate().all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                }
            })
            || report.sequence == 0
        {
            return Err("Invalid Study tab identity or sequence.");
        }
        self.tabs
            .retain(|_, tab| now.duration_since(tab.reported) < RETAIN_ORDER);
        let prior = self.tabs.get(&report.tab);
        if prior.is_some_and(|tab| tab.sequence >= report.sequence) {
            return Ok(false);
        }
        if prior.is_none() && self.tabs.len() >= MAX_TABS {
            return Err("Too many Study tabs are reporting presence.");
        }
        let active = report.state == PresenceState::Active;
        let changed_deadline = active || prior.is_none_or(|tab| tab.state == PresenceState::Active);
        let prior_deadline = prior.map_or(now, |tab| tab.blocked_until);
        let blocked_until = if changed_deadline {
            now + QUIET_PERIOD
        } else {
            prior_deadline
        };
        self.tabs.insert(
            report.tab,
            Tab {
                state: report.state,
                sequence: report.sequence,
                reported: now,
                blocked_until,
            },
        );
        Ok(changed_deadline && prior_deadline <= now)
    }

    pub(super) fn command(&mut self, now: Instant) {
        self.command_until = now + QUIET_PERIOD;
    }

    pub(super) fn waiting(&self, now: Instant) -> Option<StudyWait> {
        let mut quiet_period = self.command_until > now;
        for tab in self.tabs.values().filter(|tab| tab.blocked_until > now) {
            if tab.state == PresenceState::Active {
                return Some(StudyWait::Active);
            }
            quiet_period = true;
        }
        quiet_period.then_some(StudyWait::QuietPeriod)
    }

    #[cfg(test)]
    pub(super) fn finish_startup_wait(&mut self, now: Instant) {
        self.command_until = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(tab: u128, sequence: u64, state: PresenceState) -> PresenceReport {
        PresenceReport {
            tab: format!("00000000-0000-4000-8000-{tab:012x}"),
            sequence,
            state,
        }
    }

    #[test]
    fn separate_tabs_block_until_the_last_blur_plus_thirty_seconds() {
        use PresenceState::{Active, Inactive};
        let start = Instant::now();
        let time = |seconds| start + Duration::from_secs(seconds);
        let mut presence = StudyPresence::new(start);
        assert_eq!(presence.waiting(time(29)), Some(StudyWait::QuietPeriod));
        assert_eq!(presence.waiting(time(30)), None);
        assert!(presence.report(report(1, 1, Active), time(31)).unwrap());
        assert!(presence.report(report(2, 1, Active), time(32)).unwrap());
        assert!(!presence.report(report(1, 2, Inactive), time(40)).unwrap());
        assert!(!presence.report(report(2, 2, Active), time(50)).unwrap());
        assert_eq!(presence.waiting(time(70)), Some(StudyWait::Active));
        presence.report(report(2, 3, Inactive), time(71)).unwrap();
        presence.report(report(2, 2, Active), time(72)).unwrap();
        presence.report(report(2, 4, Inactive), time(73)).unwrap();
        assert_eq!(presence.waiting(time(100)), Some(StudyWait::QuietPeriod));
        assert_eq!(presence.waiting(time(101)), None);
        assert!(presence.report(report(2, 5, Active), time(102)).unwrap());
    }

    #[test]
    fn a_release_that_arrives_before_activation_still_starts_the_wait() {
        let start = Instant::now();
        let time = |seconds| start + Duration::from_secs(seconds);
        let mut presence = StudyPresence::new(start);
        presence
            .report(report(1, 2, PresenceState::Inactive), time(40))
            .unwrap();
        assert!(
            !presence
                .report(report(1, 1, PresenceState::Active), time(41))
                .unwrap()
        );
        assert_eq!(presence.waiting(time(69)), Some(StudyWait::QuietPeriod));
        assert_eq!(presence.waiting(time(70)), None);
    }

    #[test]
    fn lost_heartbeat_expires_without_a_second_wait_and_commands_supply_a_fallback() {
        let start = Instant::now();
        let time = |seconds| start + Duration::from_secs(seconds);
        let mut presence = StudyPresence::new(start);
        presence
            .report(report(1, 1, PresenceState::Active), time(3))
            .unwrap();
        presence
            .report(report(1, 2, PresenceState::Active), time(13))
            .unwrap();
        assert_eq!(presence.waiting(time(42)), Some(StudyWait::Active));
        assert_eq!(presence.waiting(time(43)), None);
        assert!(
            presence
                .report(report(1, 3, PresenceState::Inactive), time(44))
                .unwrap()
        );
        assert_eq!(presence.waiting(time(73)), Some(StudyWait::QuietPeriod));
        assert_eq!(presence.waiting(time(74)), None);
        presence.command(time(75));
        assert_eq!(presence.waiting(time(104)), Some(StudyWait::QuietPeriod));
        assert_eq!(presence.waiting(time(105)), None);
    }
}
