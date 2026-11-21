//! Read and atomically update the installed app's tactic preferences.
use super::Profile;
use serde::Deserialize;
use serde_json::{Value, json};
use crate::{AppError, TacticThemes, Tempo, tactic_themes};
use tempo_store::{SqliteStore, StoreError};
use tempo_types::DatasetId;

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Command {
    Get,
    Save {
        dataset_id: DatasetId,
        expected_revision: String,
        new_tactics_per_day: usize,
        selection: TacticThemes,
    },
}

pub(super) fn handle(
    app: &mut Tempo,
    store: &SqliteStore,
    command: Command,
) -> Result<Value, AppError> {
    let _trace = crate::diagnostics::Span::new("settings.tactics");
    let profile_error = || {
        AppError::new(
            "invalid_profile",
            "Tempo could not read its Study settings.",
        )
    };
    let conflict = || {
        AppError::new(
            "settings_conflict",
            "Settings changed in another session. Reload settings before saving.",
        )
    };
    let mut raw = store.local_profile()?.ok_or_else(profile_error)?;
    let mut profile: Profile = serde_json::from_str(&raw).map_err(|_| profile_error())?;
    app.set_local_tactic_preferences(profile.introductions.tactics, profile.tactic_themes.clone());
    let (dataset, mut selection) = app.local_tactic_themes()?;
    let revision = |profile: &str| blake3::hash(profile.as_bytes()).to_hex().to_string();
    if let Command::Save {
        dataset_id,
        expected_revision,
        new_tactics_per_day,
        selection: requested,
    } = command
    {
        if dataset != dataset_id {
            return Err(AppError::new(
                "stale_dataset",
                "Reload settings for the current Study dataset.",
            ));
        }
        if (new_tactics_per_day as u64) > 9_007_199_254_740_991 || !requested.valid() {
            return Err(AppError::new(
                "invalid_settings",
                "Enter a nonnegative whole number and choose at least one theme or Any tactic.",
            ));
        }
        let already_saved = profile.introductions.tactics == new_tactics_per_day
            && profile.tactic_themes.as_ref() == Some(&requested);
        if !already_saved {
            if expected_revision != revision(&raw) {
                return Err(conflict());
            }
            profile.introductions.tactics = new_tactics_per_day;
            profile.tactic_themes = Some(requested.clone());
            let updated = serde_json::to_string(&profile).map_err(|_| profile_error())?;
            store
                .update_local_profile(&raw, &updated)
                .map_err(|error| match error {
                    StoreError::Conflict(_) => conflict(),
                    _ => error.into(),
                })?;
            raw = updated;
            selection = requested;
            app.set_local_tactic_preferences(new_tactics_per_day, profile.tactic_themes.clone());
        }
    }
    Ok(json!({
        "schema_version": 1,
        "status": "tactic_settings",
        "dataset_id": dataset,
        "revision": revision(&raw),
        "new_tactics_per_day": profile.introductions.tactics,
        "selection": selection,
        "themes": tactic_themes(),
        "starter": TacticThemes::starter(),
    }))
}
