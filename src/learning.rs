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
