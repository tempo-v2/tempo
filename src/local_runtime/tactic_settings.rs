//! Read and atomically update the installed app's tactic preferences.
use super::Profile;
use crate::{AppError, TacticThemes, Tempo, tactic_themes};
use serde::Deserialize;
use serde_json::{Value, json};
use tempo_store::{SqliteStore, StoreError, StudyProfilePlanEffect};
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
            let plan_effect = if profile.introductions.tactics == new_tactics_per_day
                && profile.tactic_themes.as_ref().unwrap_or(&TacticThemes::Any) == &requested
            {
                StudyProfilePlanEffect::Preserve
            } else {
                StudyProfilePlanEffect::InvalidateMembership
            };
            profile.introductions.tactics = new_tactics_per_day;
            profile.tactic_themes = Some(requested.clone());
            let updated = serde_json::to_string(&profile).map_err(|_| profile_error())?;
            store
                .update_study_profile_for_plan(&dataset, &raw, &updated, plan_effect)
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempo_types::StudyDay;

    #[test]
    fn installed_settings_handler_fences_policy_changes_but_not_explicit_default() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.sqlite");
        super::super::init(&path).unwrap();
        let mut app = super::super::open(&path).unwrap();
        let mut store = SqliteStore::open(&path).unwrap();
        let initial = handle(&mut app, &store, Command::Get).unwrap();
        let dataset: DatasetId = serde_json::from_value(initial["dataset_id"].clone()).unwrap();
        let limit = Profile::default().introductions.tactics;
        let before = store
            .observe_study_day(&dataset, StudyDay(100))
            .unwrap()
            .unwrap();

        let save = |app: &mut Tempo, previous: &Value, limit, selection| {
            handle(
                app,
                &store,
                Command::Save {
                    dataset_id: dataset.clone(),
                    expected_revision: previous["revision"].as_str().unwrap().to_owned(),
                    new_tactics_per_day: limit,
                    selection,
                },
            )
            .unwrap()
        };
        let normalized = save(&mut app, &initial, limit, TacticThemes::Any);
        assert_ne!(normalized["revision"], initial["revision"]);
        assert_eq!(
            store.study_projection_basis(&dataset).unwrap().as_ref(),
            Some(&before)
        );

        let changed = save(&mut app, &normalized, limit + 1, TacticThemes::Any);
        assert_eq!(changed["new_tactics_per_day"], limit + 1);
        let after_limit = store.study_projection_basis(&dataset).unwrap().unwrap();
        assert_eq!(
            after_limit.plan.membership_epoch.0,
            before.plan.membership_epoch.0 + 1
        );
        let selection = TacticThemes::starter();
        let changed = save(&mut app, &changed, limit + 1, selection.clone());
        let replay = save(&mut app, &normalized, limit + 1, selection);
        assert_eq!(changed, replay);
        let after = store.study_projection_basis(&dataset).unwrap().unwrap();
        assert_eq!(
            after.plan.membership_epoch.0,
            after_limit.plan.membership_epoch.0 + 1
        );
        assert_eq!(after.plan.rank_revision, before.plan.rank_revision);
    }
}
