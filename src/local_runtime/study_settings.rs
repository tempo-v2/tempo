//! Read and save the editable part of the installed desktop Study profile.
use super::Profile;
use crate::{
    AppError, IntroductionLimits, TacticThemes, Tempo,
    application::{MAX_STUDY_COUNT, TacticTheme},
    tactic_themes,
};
use serde::{Deserialize, Serialize};
use tempo_store::{SqliteStore, Store, StoreError, StudyScope};
use tempo_types::DatasetId;

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Command {
    Get,
    Save {
        dataset_id: DatasetId,
        expected_revision: String,
        introductions: IntroductionLimits,
        more_tactics: usize,
        #[serde(deserialize_with = "Deserialize::deserialize")]
        selection: Option<TacticThemes>,
    },
}

#[derive(Serialize)]
pub(super) struct SettingsView {
    schema_version: u32,
    status: &'static str,
    dataset_id: DatasetId,
    revision: String,
    study_timezone: String,
    introductions: IntroductionLimits,
    more_tactics: usize,
    selection: Option<TacticThemes>,
    themes: &'static [TacticTheme],
    starter: TacticThemes,
}

pub(super) fn handle(
    app: &mut Tempo,
    store: &SqliteStore,
    command: Command,
) -> Result<SettingsView, AppError> {
    let profile_error = || {
        AppError::new(
            "invalid_profile",
            "Tempo could not read its Study settings.",
        )
    };
    let conflict = || {
        AppError::new(
            "settings_conflict",
            "Settings changed in another session. Reload saved values before saving.",
        )
    };
    let mut raw = store.local_profile()?.ok_or_else(profile_error)?;
    let mut profile: Profile = serde_json::from_str(&raw).map_err(|_| profile_error())?;
    let dataset = store.study_state(StudyScope::default())?.dataset;
    let revision = |profile: &str| blake3::hash(profile.as_bytes()).to_string();

    if let Command::Save {
        dataset_id,
        expected_revision,
        introductions,
        more_tactics,
        selection,
    } = command
    {
        if dataset != dataset_id {
            return Err(AppError::new(
                "stale_dataset",
                "Reload settings for the current Study dataset.",
            ));
        }
        if profile.introductions != introductions
            || profile.more_tactics != more_tactics
            || profile.tactic_themes != selection
        {
            if expected_revision != revision(&raw) {
                return Err(conflict());
            }
            if [
                introductions.repertoire,
                introductions.tactics,
                more_tactics,
            ]
            .into_iter()
            .any(|limit| limit > MAX_STUDY_COUNT)
                || selection.as_ref().is_some_and(|themes| !themes.valid())
            {
                return Err(AppError::new(
                    "invalid_settings",
                    "Enter nonnegative whole numbers and select at least one theme when using selected themes.",
                ));
            }
            profile.introductions = introductions;
            profile.more_tactics = more_tactics;
            profile.tactic_themes = selection;
            let updated = serde_json::to_string(&profile).map_err(|_| profile_error())?;
            store
                .update_local_profile(&raw, &updated)
                .map_err(|error| match error {
                    StoreError::Conflict(_) => conflict(),
                    _ => error.into(),
                })?;
            raw = updated;
            profile.apply_study_settings(app);
        }
    }

    Ok(SettingsView {
        schema_version: 1,
        status: "study_settings",
        dataset_id: dataset,
        revision: revision(&raw),
        study_timezone: profile.study_timezone,
        introductions: profile.introductions,
        more_tactics: profile.more_tactics,
        selection: profile.tactic_themes,
        themes: tactic_themes(),
        starter: TacticThemes::starter(),
    })
}
