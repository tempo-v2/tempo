//! Read and atomically update the installed app’s shared Study settings.
use super::Profile;
use crate::{AppError, TacticThemes, Tempo, tactic_themes};
use serde::Deserialize;
use serde_json::{Value, json};
use tempo_store::{SqliteStore, StoreError, StudyProfilePlanEffect};
use tempo_types::DatasetId;

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Command {
    Get,
    Save {
        dataset_id: DatasetId,
        expected_revision: String,
        new_tactics_per_day: usize,
        new_openings_per_day: Option<usize>,
        selection: TacticThemes,
    },
}

pub(crate) fn handle(
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
    app.apply_local_study_settings(profile.introductions, profile.tactic_themes.clone());
    let (dataset, mut selection) = app.local_tactic_themes()?;
    let revision = |profile: &str| blake3::hash(profile.as_bytes()).to_hex().to_string();
    if let Command::Save {
        dataset_id,
        expected_revision,
        new_tactics_per_day,
        new_openings_per_day,
        selection: requested,
    } = command
    {
        if dataset != dataset_id {
            return Err(AppError::new(
                "stale_dataset",
                "Reload settings for the current Study dataset.",
            ));
        }
        const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
        if (new_tactics_per_day as u64) > MAX_SAFE_INTEGER
            || new_openings_per_day.is_some_and(|limit| (limit as u64) > MAX_SAFE_INTEGER)
            || !requested.valid()
        {
            return Err(AppError::new(
                "invalid_settings",
                "Enter a nonnegative whole number and choose at least one theme or Any tactic.",
            ));
        }
        let openings = new_openings_per_day.unwrap_or(profile.introductions.repertoire);
        let limits_changed = profile.introductions.repertoire != openings
            || profile.introductions.tactics != new_tactics_per_day;
        let already_saved = !limits_changed && profile.tactic_themes.as_ref() == Some(&requested);
        if !already_saved {
            if expected_revision != revision(&raw) {
                return Err(conflict());
            }
            let plan_effect = if !limits_changed
                && profile.tactic_themes.as_ref().unwrap_or(&TacticThemes::Any) == &requested
            {
                StudyProfilePlanEffect::Preserve
            } else {
                StudyProfilePlanEffect::InvalidateMembership
            };
            profile.introductions.repertoire = openings;
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
            app.apply_local_study_settings(profile.introductions, profile.tactic_themes.clone());
        }
    }
    Ok(json!({
        "schema_version": 1,
        "status": "tactic_settings",
        "dataset_id": dataset,
        "revision": revision(&raw),
        "new_openings_per_day": profile.introductions.repertoire,
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
        let profile: Profile =
            serde_json::from_str(&store.local_profile().unwrap().unwrap()).unwrap();
        let limit = profile.introductions.tactics;
        let before = store
            .observe_study_calendar_day(&dataset, &profile.study_timezone, StudyDay(100))
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
                    new_openings_per_day: None,
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

    #[test]
    fn opening_limit_on_live_settings_survives_legacy_tactic_save_and_preserves_profile() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.sqlite");
        let profile = Profile {
            study_timezone: "America/Chicago".into(),
            preview_key: [17; 32],
            more_tactics: 7,
            ..Profile::default()
        };
        let dataset = DatasetId::from("settings-authority");
        use tempo_learning::{DailyLearning, LearningPolicy};
        let mut store = SqliteStore::create_with_profile(
            &path,
            dataset.clone(),
            tempo_chess::Catalog::default(),
            DailyLearning::default().settings(),
            Some(&serde_json::to_string(&profile).unwrap()),
        )
        .unwrap();
        let mut app = super::super::open(&path).unwrap();
        let before = store
            .observe_study_calendar_day(&dataset, &profile.study_timezone, StudyDay(100))
            .unwrap()
            .unwrap();
        let original = handle(&mut app, &store, Command::Get).unwrap();
        let save = |previous: &Value, openings, tactics| Command::Save {
            dataset_id: dataset.clone(),
            expected_revision: previous["revision"].as_str().unwrap().into(),
            new_openings_per_day: openings,
            new_tactics_per_day: tactics,
            selection: TacticThemes::Any,
        };
        let tactics = profile.introductions.tactics;
        let changed = handle(&mut app, &store, save(&original, Some(0), tactics)).unwrap();
        assert_eq!(changed["new_openings_per_day"], 0);
        let after = store.study_projection_basis(&dataset).unwrap().unwrap();
        assert_eq!(
            after.plan.membership_epoch.0,
            before.plan.membership_epoch.0 + 1
        );
        assert_eq!(after.summary.epoch.0, before.summary.epoch.0 + 1);
        let repeated = handle(&mut app, &store, save(&original, Some(0), tactics)).unwrap();
        assert_eq!(changed, repeated);
        assert_eq!(
            store.study_projection_basis(&dataset).unwrap().as_ref(),
            Some(&after)
        );
        assert!(handle(&mut app, &store, save(&original, Some(2), tactics)).is_err());
        let legacy = handle(&mut app, &store, save(&changed, None, tactics + 1)).unwrap();
        assert_eq!(legacy["new_openings_per_day"], 0);
        drop(app);
        drop(store);
        let mut app = super::super::open(&path).unwrap();
        let store = SqliteStore::open(&path).unwrap();
        let restored = handle(&mut app, &store, Command::Get).unwrap();
        assert_eq!(restored, legacy);
        let persisted: Profile =
            serde_json::from_str(&store.local_profile().unwrap().unwrap()).unwrap();
        assert_eq!(persisted.study_timezone, profile.study_timezone);
        assert_eq!(persisted.preview_key, profile.preview_key);
        assert_eq!(persisted.more_tactics, profile.more_tactics);
    }
}
