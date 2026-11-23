//! Cross-component journeys using the public commands of Daily Study.
//!
//! Keep `cargo test -p tempo --test contracts` stable. Use a module name as a
//! filter to read/run a specific journey, e.g. `--test contracts content_updates`.
#[path = "contracts/durable_repertoire.rs"]
mod durable_repertoire;
#[path = "contracts/opening_release.rs"]
mod opening_release;

#[path = "contracts/portable_restore.rs"]
mod portable_restore;
#[path = "contracts/study_journeys.rs"]
mod study_journeys;
#[path = "contracts/submission_recovery.rs"]
mod submission_recovery;
#[path = "contracts/support.rs"]
mod support;
#[path = "contracts/tactics_mvp.rs"]
mod tactics_mvp;
