//! Ports for the Daily Study application.
use crate::{AttemptIds, ChessGrader, Clock};
use tempo_learning::LearningPolicy;

/// Ports consumed by Daily Study and signed content and Games operations.
pub struct DailyComponents {
    pub learning: Box<dyn LearningPolicy>,
    pub grader: Box<ChessGrader>,
    pub clock: Box<dyn Clock>,
    pub ids: Box<dyn AttemptIds>,
}

#[cfg(test)]
pub(crate) fn test_components() -> DailyComponents {
    DailyComponents {
        learning: Box::new(tempo_learning::DailyLearning::default()),
        grader: Box::new(tempo_chess::LegalSequenceGrader),
        clock: Box::new(crate::FixedClock(tempo_types::StudyTime {
            day: tempo_types::StudyDay(10),
            unix_ms: 864_000_000,
        })),
        ids: Box::new(crate::CounterIds::default()),
    }
}

#[cfg(test)]
pub(crate) fn test_store(
    directory: &tempfile::TempDir,
    dataset: &str,
    catalog: tempo_chess::Catalog,
    components: &DailyComponents,
) -> tempo_store::SqliteStore {
    tempo_store::SqliteStore::create(
        directory.path().join("study.sqlite"),
        dataset.into(),
        catalog,
        components.learning.settings(),
    )
    .unwrap()
}

#[cfg(test)]
pub(crate) fn test_study() -> (crate::Tempo, tempfile::TempDir) {
    use tempo_learning::{DailyLearning, LearningState};
    use tempo_store::{Commit, Store, Write};
    use tempo_types::{StudyDay, StudyTime};

    let components = test_components();
    let directory = tempfile::tempdir().unwrap();
    let mut store = test_store(
        &directory,
        "unit-study",
        tempo_chess::demo_catalog(),
        &components,
    );
    let state = store.snapshot().unwrap();
    store
        .commit(Commit {
            expected_dataset: state.dataset,
            expected_revision: state.revision,
            writes: vec![Write::Learning {
                target: "r1:p1".into(),
                state: DailyLearning::teach(
                    &LearningState::default(),
                    StudyTime {
                        day: StudyDay(9),
                        unix_ms: 777_600_000,
                    },
                )
                .unwrap(),
            }],
        })
        .unwrap();
    let app = crate::Tempo::daily(
        Box::new(store),
        components,
        crate::IntroductionLimits::default(),
        [42; 32],
        10,
    )
    .unwrap();
    (app, directory)
}
