//! Archived public-game command values.
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Deserialize, serde::Serialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Provider {
    Lichess,
    ChessCom,
}

pub(crate) fn valid_account(account: &str) -> bool {
    !account.is_empty()
        && account.len() <= 40
        && account
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}
