//! Project original pack membership into an order for new tactics only.
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};
use tempo_queue::TacticPackCandidate;
use tempo_types::{StudyDay, TargetId};

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

#[derive(Deserialize, Serialize)]
pub struct TacticTheme {
    pub id: String,
    name: String,
    group: String,
}

pub fn tactic_themes() -> &'static [TacticTheme] {
    static THEMES: LazyLock<Vec<TacticTheme>> = LazyLock::new(|| {
        serde_json::from_str(include_str!("tactic_themes.json")).expect("bundled Tempo theme list")
    });
    &THEMES
}

pub(super) fn known_tactic_theme(id: &str) -> bool {
    static IDS: LazyLock<BTreeSet<&'static str>> = LazyLock::new(|| {
        tactic_themes()
            .iter()
            .map(|theme| theme.id.as_str())
            .collect()
    });
    IDS.contains(id)
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
                !theme_ids.is_empty()
                    && theme_ids
                        .iter()
                        .all(|id| tactic_themes().iter().any(|theme| &theme.id == id))
            }
        }
    }
}

fn has_starter_packs<'a>(mut candidates: impl Iterator<Item = TacticPackCandidate<'a>>) -> bool {
    candidates.any(|candidate| {
        candidate
            .memberships
            .iter()
            .any(|pack| STARTER.contains(&pack.theme))
    })
}

pub(super) fn new_tactic_order<'a>(
    candidates: impl Iterator<Item = TacticPackCandidate<'a>> + Clone,
    today: StudyDay,
    preference: Option<&TacticThemes>,
    use_starter: bool,
) -> Option<BTreeMap<TargetId, usize>> {
    let starter;
    let preference = match preference {
        Some(preference) => preference,
        None if use_starter && has_starter_packs(candidates.clone()) => {
            starter = TacticThemes::starter();
            &starter
        }
        None => return None,
    };
    let TacticThemes::Selected { theme_ids } = preference else {
        return None;
    };
    let themes: Vec<_> = tactic_themes()
        .iter()
        .filter(|theme| theme_ids.contains(&theme.id))
        .map(|theme| theme.id.as_str())
        .collect();
    tempo_queue::rank_new_tactics(candidates, today, Some(&themes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::tactic_catalog_facts::tactic_pack_candidates;
    use crate::Tempo;
    use tempo_learning::LearningState;
    use tempo_store::Snapshot;

    #[test]
    fn first_introductions_rotate_by_day_and_prefer_earlier_packs() {
        let mut snapshot = Tempo::demo().unwrap().snapshot().unwrap();
        let template = snapshot
            .catalog
            .items
            .values()
            .find(|item| !item.definition.is_repertoire())
            .unwrap()
            .clone();
        snapshot.catalog.items.clear();
        snapshot.learning.clear();
        let entries = [
            (
                "fork-first",
                vec![("fork", "easy", 1, 2), ("hangingPiece", "hard", 1, 1)],
            ),
            ("fork-medium", vec![("fork", "medium", 1, 1)]),
            ("fork-hard", vec![("fork", "hard", 1, 1)]),
            ("fork-focused", vec![("fork", "focused", 1, 1)]),
            ("hanging-first", vec![("hangingPiece", "easy", 1, 1)]),
            ("hanging-later", vec![("hangingPiece", "easy", 2, 1)]),
            ("other", vec![("pin", "easy", 1, 1)]),
            ("capture", vec![]),
        ];
        for (id, metadata) in entries {
            let target = TargetId::from(id);
            let mut item = template.clone();
            item.target = target.clone();
            item.provenance = metadata.into_iter().map(|(motif, difficulty, ordinal, position)| {
                format!("tempo1:catalog-metadata:{}", serde_json::json!({
                    "Motif": motif, "Difficulty": difficulty, "DeckId": format!("{motif}-{difficulty}-{ordinal:02}"),
                    "DeckPosition": position,
                }))
            }).collect();
            snapshot.catalog.items.insert(target.clone(), item);
            snapshot.learning.insert(target, LearningState::default());
        }
        let preference = TacticThemes::Selected {
            theme_ids: ["fork".into(), "hangingPiece".into()].into(),
        };
        let order = |snapshot: &Snapshot, day| {
            let mut ranked = new_tactic_order(
                tactic_pack_candidates(&snapshot.catalog, &snapshot.learning),
                StudyDay(day),
                Some(&preference),
                true,
            )
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>();
            ranked.sort_by_key(|(_, order)| *order);
            ranked
                .into_iter()
                .map(|(target, _)| target.0)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            order(&snapshot, 1),
            [
                "fork-first",
                "fork-medium",
                "fork-hard",
                "hanging-first",
                "hanging-later"
            ]
        );
        assert_eq!(order(&snapshot, 2)[0], "hanging-first");
        let original_order = order(&snapshot, 1);
        for item in snapshot.catalog.items.values_mut() {
            item.label = "Renamed puzzle".into();
            item.provenance.push("unrelated attribution".into());
        }
        assert_eq!(order(&snapshot, 1), original_order);
        snapshot
            .learning
            .get_mut(&TargetId::from("fork-first"))
            .unwrap()
            .first_introduced = Some(StudyDay(1));
        assert_eq!(
            order(&snapshot, 1),
            ["hanging-first", "hanging-later", "fork-medium", "fork-hard"]
        );
    }
}
