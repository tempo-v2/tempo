//! Tempo's composition root: intent in, domain decisions, one persisted result.
//!
//! # Where to start
//! - [`Tempo`]: public application operations (`src/application/`).
//! - [`commands`]: caller-owned intent and retry identities.
//! - [`DailyComponents`] and [`ports`]: choose replaceable policy/adapter implementations.
//! - [`views`] and [`wire`]: the versioned UI boundary, with no answer keys.
//!
//! Each concept's source file explains when to use it and keeps focused examples
//! beside the definition. `tests/contracts.rs` retains the cross-component journeys.
pub mod adapters;
pub mod app_error;
mod application;
pub mod commands;
pub mod components;
mod diagnostics;
pub mod game_range;
pub mod ports;
pub mod views;
pub mod wire;

pub use adapters::{CounterIds, FixedClock};
pub use app_error::AppError;
pub use application::{
    GameLibraryRead, GameReviewLine, GameReviewRead, IntroductionLimits, ProgressStamp,
    StudyAnalysisLine, StudyProgress, TacticThemes, TacticsRead, TacticsReadReply, Tempo,
    tactic_themes,
};
pub use application::{SaveStudyReview, SavedStudyReviewCommand};
pub use commands::{AbandonCommand, SubmitCommand};
pub use components::DailyComponents;
pub use game_range::GameRange;
pub use ports::{AttemptIds, ChessGrader, Clock};

pub mod game_evidence;

pub mod game_backfill;
pub mod game_model;

mod maia;

pub mod local_runtime;
