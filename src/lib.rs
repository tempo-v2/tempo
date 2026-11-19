//! Tempo's composition root: intent in, domain decisions, one persisted result.
//!
//! # Where to start
//! - [`Tempo`]: public application operations (`src/application/`).
//! - [`commands`]: caller-owned intent and retry identities.
//! - [`Components`] and [`ports`]: choose replaceable policy/adapter implementations.
//! - [`adapters`]: executable fixture implementations of those ports.
//! - [`views`] and [`wire`]: the versioned UI boundary, with no answer keys.
//! - [`PortableSnapshot`]: export/restore data, separate from the store snapshot.
//!
//! Each concept's source file explains when to use it and keeps focused examples
//! beside the definition. `tests/contracts.rs` retains the cross-component journeys.
pub mod adapters;
pub mod app_error;
mod application;
pub mod commands;
pub mod components;
pub mod game;
pub mod game_range;
pub mod portable_snapshot;
pub mod ports;
pub mod views;
pub mod wire;

pub use adapters::{CounterIds, FixedClock, FixtureGames, JsonSnapshotCodec, UnavailableGames};
pub use app_error::AppError;
pub use application::{
    IntroductionLimits, ProgressStamp, StudyProgress, TacticThemes, TacticsRead, TacticsReadReply,
    Tempo, tactic_themes,
};
pub use commands::{AbandonCommand, AgainCommand, SubmitCommand};
pub use components::Components;
pub use game::Game;
pub use game_range::GameRange;
pub use portable_snapshot::PortableSnapshot;
pub use ports::{AttemptIds, ChessGrader, Clock, GameProvider, SnapshotCodec};

pub mod game_evidence;

pub mod game_backfill;
pub mod game_model;
