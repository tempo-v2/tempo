//! Cross-component journeys using the same public commands as the stdio adapter.
//!
//! Keep `cargo test -p tempo --test contracts` stable. Use a module name as a
//! filter to read/run a specific journey, e.g. `--test contracts content_updates`.
#[path = "contracts/content_updates.rs"]
mod content_updates;
#[path = "contracts/durable_repertoire.rs"]
mod durable_repertoire;
#[path = "contracts/external_feedback.rs"]
mod external_feedback;
#[path = "contracts/portable_restore.rs"]
mod portable_restore;
#[path = "contracts/queue_substitution.rs"]
mod queue_substitution;
#[path = "contracts/study_journeys.rs"]
mod study_journeys;
#[path = "contracts/submission_recovery.rs"]
mod submission_recovery;
#[path = "contracts/support.rs"]
mod support;
