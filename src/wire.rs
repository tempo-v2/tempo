//! Version 4 UI contract: intent enters, displayable view leaves.
//!
//! Commands live in [`crate::commands`], views in [`crate::views`], and dispatch
//! in `application/handle.rs`. These re-exports preserve the transport-facing
//! `tempo::wire::{StudyCommand, StudyView}` paths.
pub use crate::commands::StudyCommand;
pub use crate::views::{AttemptView, EmptyView, ErrorView, StudyView};
pub(crate) const STUDY_WIRE_VERSION: u32 = 4;
pub use crate::commands::RepertoireCommand;
