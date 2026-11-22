//! Apply the saved theme preference to typed new-tactic admission facts.

use super::Tempo;
use crate::AppError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
pub use tempo_chess::tactic_themes;
use tempo_chess::{TacticPolicyFacts, known_tactic_theme};

const STARTER: [&str; 7] = [
    "hangingPiece",
    "mateIn1",
    "mateIn2",
    "fork",
    "pin",
    "skewer",
    "discoveredAttack",
];

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum TacticThemes {
    Any,
    Selected { theme_ids: BTreeSet<String> },
}

impl TacticThemes {
    pub fn starter() -> Self {
        Self::Selected {
            theme_ids: STARTER.into_iter().map(String::from).collect(),
        }
    }

    pub fn valid(&self) -> bool {
        match self {
            Self::Any => true,
            Self::Selected { theme_ids } => {
                !theme_ids.is_empty() && theme_ids.iter().all(|id| known_tactic_theme(id))
            }
        }
    }
    /// Membership gates new tactics only. An unset or empty fact cannot match a
    /// selected theme.
    pub(super) fn admits(&self, facts: Option<&TacticPolicyFacts>) -> bool {
        match self {
            Self::Any => true,
            Self::Selected { theme_ids } => {
                facts.is_some_and(|facts| !facts.theme_ids().is_disjoint(theme_ids))
            }
        }
    }
}

impl Tempo {
    pub(crate) fn local_study_profile(&self) -> Result<Option<String>, AppError> {
        Ok(self.store.local_profile()?)
    }

    pub(crate) fn save_local_study_profile(
        &mut self,
        dataset: &tempo_types::DatasetId,
        expected: &str,
        replacement: &str,
        effect: tempo_store::StudyProfilePlanEffect,
    ) -> Result<(), tempo_store::StoreError> {
        self.store
            .update_study_profile_for_plan(dataset, expected, replacement, effect)
    }

    pub(crate) fn apply_local_study_settings(
        &mut self,
        limits: crate::IntroductionLimits,
        themes: Option<TacticThemes>,
    ) {
        if let Some(daily) = &mut self.daily {
            daily.limits = limits;
            daily.tactic_themes = themes;
        }
    }

    pub fn set_local_tactic_preferences(&mut self, limit: usize, themes: Option<TacticThemes>) {
        let _diagnostic =
            crate::diagnostics::Span::new("app.tactic_preferences.set_local_tactic_preferences");
        if let Some(daily) = &mut self.daily {
            daily.limits.tactics = limit;
            daily.tactic_themes = themes;
        }
    }

    pub fn local_tactic_themes(
        &self,
    ) -> Result<(tempo_types::DatasetId, TacticThemes), crate::AppError> {
        let _diagnostic =
            crate::diagnostics::Span::new("app.tactic_preferences.local_tactic_themes");
        let themes = self
            .daily
            .as_ref()
            .and_then(|daily| daily.tactic_themes.clone())
            .unwrap_or(TacticThemes::Any);
        Ok((self.store.dataset_id()?, themes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FixedClock, IntroductionLimits,
        components::{test_components, test_store},
    };
    use tempo_chess::{Catalog, START_FEN, TacticRecord, compile_tactics};
    use tempo_types::{StudyDay, StudyTime, TargetId};

    fn marker(theme: &str, difficulty: &str, ordinal: usize) -> String {
        format!(
            "tempo1:catalog-metadata:{}",
            serde_json::json!({
                "Motif": theme,
                "Difficulty": difficulty,
                "DeckId": format!("{theme}-{difficulty}-{ordinal:02}"),
                "DeckPosition": 1,
            }),
        )
    }

    fn catalog_with_theme_facts() -> (Catalog, [TargetId; 5]) {
        let records = [
            ("a-missing", "e2e4"),
            ("b-empty", "d2d4"),
            ("c-pin", "c2c4"),
            ("d-first-fork", "g1f3"),
            ("e-second-fork", "g2g3"),
        ]
        .map(|(source, played)| TacticRecord {
            namespace: "theme-test".into(),
            source_id: source.into(),
            solver_fen: START_FEN.into(),
            solution: vec![played.into()],
            label: source.into(),
            // This marker deliberately contradicts some authoritative facts.
            provenance: vec![marker("fork", "easy", 1)],
        });
        let ids = records.each_ref().map(TacticRecord::target_id);
        let mut catalog = compile_tactics(&records).unwrap();
        for (target, facts) in ids.iter().zip([
            None,
            Some(TacticPolicyFacts::default()),
            Some(TacticPolicyFacts::from_legacy_metadata([marker(
                "pin", "easy", 1,
            )
            .as_str()])),
            Some(TacticPolicyFacts::from_legacy_metadata([marker(
                "fork", "focused", 4,
            )
            .as_str()])),
            Some(TacticPolicyFacts::from_legacy_metadata([marker(
                "fork", "easy", 1,
            )
            .as_str()])),
        ]) {
            catalog.items.get_mut(target).unwrap().tactic_policy_facts = facts;
        }
        (catalog, ids)
    }

    fn selected(theme: &str) -> TacticThemes {
        TacticThemes::Selected {
            theme_ids: [theme.to_owned()].into(),
        }
    }

    fn issued_target(catalog: Catalog, day: i32, themes: Option<TacticThemes>) -> TargetId {
        let directory = tempfile::tempdir().unwrap();
        let mut components = test_components();
        components.clock = Box::new(FixedClock(StudyTime {
            day: StudyDay(day),
            unix_ms: day as u64 * 86_400_000,
        }));
        let store = test_store(&directory, "themes-test", catalog, &components);
        let limits = IntroductionLimits {
            repertoire: 0,
            tactics: 1,
        };
        let mut app = Tempo::daily(Box::new(store), components, limits, [42; 32], 0).unwrap();
        assert_eq!(
            app.local_tactic_themes().unwrap(),
            ("themes-test".into(), TacticThemes::Any)
        );
        app.set_local_tactic_preferences(limits.tactics, themes.clone());
        assert_eq!(
            app.local_tactic_themes().unwrap().1,
            themes.unwrap_or(TacticThemes::Any),
        );
        app.next_exercise().unwrap().unwrap().attempt.target
    }

    #[test]
    fn next_admits_typed_themes_in_target_order_across_day_and_attribution_changes() {
        let (mut catalog, [_, _, pin, first_fork, second_fork]) = catalog_with_theme_facts();
        assert_eq!(
            issued_target(catalog.clone(), 10, Some(selected("fork"))),
            first_fork,
        );
        // Deck order, labels and provenance do not influence the typed order.
        for item in catalog.items.values_mut() {
            item.label = "Relabelled tactic".into();
            item.provenance = vec![marker("pin", "easy", 2)];
        }
        assert_eq!(
            issued_target(catalog.clone(), 11, Some(selected("fork"))),
            first_fork,
        );
        assert_eq!(
            issued_target(catalog.clone(), 11, Some(selected("pin"))),
            pin,
        );
        catalog
            .items
            .get_mut(&first_fork)
            .unwrap()
            .tactic_policy_facts = Some(TacticPolicyFacts::default());
        assert_eq!(
            issued_target(catalog, 11, Some(selected("fork"))),
            second_fork
        );
    }

    #[test]
    fn unconfigured_and_any_admit_missing_or_explicitly_empty_membership() {
        let (mut catalog, [missing, empty, ..]) = catalog_with_theme_facts();
        assert_eq!(issued_target(catalog.clone(), 10, None), missing);
        assert_eq!(
            issued_target(catalog.clone(), 11, Some(TacticThemes::Any)),
            missing,
        );
        catalog.items.remove(&missing);
        assert_eq!(issued_target(catalog, 11, Some(TacticThemes::Any)), empty);
    }
}
