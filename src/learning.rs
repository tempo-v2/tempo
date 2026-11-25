use anyhow::Result;
use chrono::{DateTime, Local};
use fsrs::{FSRS, MemoryState};

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum Rating {
    Good,
    Again,
}

impl Rating {
    pub fn label(self) -> &'static str {
        match self {
            Self::Good => "Good",
            Self::Again => "Again",
        }
    }
}

pub struct PriorReview {
    pub stability: f32,
    pub difficulty: f32,
    pub last_at: i64,
}

pub struct ReviewSchedule {
    pub stability: f32,
    pub difficulty: f32,
    pub due_at: i64,
}

pub fn schedule(rating: Rating, prior: Option<PriorReview>, now: i64) -> Result<ReviewSchedule> {
    let elapsed = prior.as_ref().map_or(0, |prior| {
        let date = |time| {
            DateTime::from_timestamp(time, 0)
                .unwrap()
                .with_timezone(&Local)
                .date_naive()
        };
        (date(now) - date(prior.last_at)).num_days().max(0) as u32
    });
    let memory = prior.map(|prior| MemoryState {
        stability: prior.stability,
        difficulty: prior.difficulty,
    });
    let states = FSRS::default().next_states(memory, 0.9, elapsed)?;
    let item = match rating {
        Rating::Good => states.good,
        Rating::Again => states.again,
    };
    let due_at = now + i64::from(item.interval.round().max(1.0) as u32) * 86_400;
    Ok(ReviewSchedule {
        stability: item.memory.stability,
        difficulty: item.memory.difficulty,
        due_at,
    })
}

pub fn draw_pack_days() -> u8 {
    use rand::Rng;
    rand::rng().random_range(7..=20)
}

pub fn first_pack_due(now: i64, days: u8) -> Result<(i64, String)> {
    due_in_zone(now, days, &Local)
}

fn due_in_zone<T: chrono::TimeZone>(now: i64, days: u8, zone: &T) -> Result<(i64, String)> {
    use chrono::Days;
    anyhow::ensure!((7..=20).contains(&days), "Invalid first pack interval");
    let today = DateTime::from_timestamp(now, 0)
        .ok_or_else(|| anyhow::anyhow!("Invalid acceptance time"))?
        .with_timezone(zone)
        .date_naive();
    let day = today
        .checked_add_days(Days::new(u64::from(days)))
        .ok_or_else(|| anyhow::anyhow!("Invalid due day"))?;
    let midnight = day.and_hms_opt(0, 0, 0).unwrap();
    for minute in 0..2880 {
        let local = midnight + chrono::Duration::minutes(minute);
        if let Some(due) = zone.from_local_datetime(&local).earliest() {
            return Ok((due.timestamp(), day.to_string()));
        }
    }
    anyhow::bail!("No representable local due instant")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn pack_due_date_uses_study_days_at_both_ends_of_the_draw() -> Result<()> {
        let zone = chrono_tz::America::New_York;
        let now = zone
            .with_ymd_and_hms(2026, 3, 6, 23, 30, 0)
            .single()
            .unwrap()
            .timestamp();
        for (days, date, day_of_month) in [(7, "2026-03-13", 13), (20, "2026-03-26", 26)] {
            let (due, day) = due_in_zone(now, days, &zone)?;
            assert_eq!(day, date);
            assert_eq!(
                due,
                Utc.with_ymd_and_hms(2026, 3, day_of_month, 4, 0, 0)
                    .single()
                    .unwrap()
                    .timestamp()
            );
        }
        Ok(())
    }
}
