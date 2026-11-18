//! Shared setup for the external contract journeys; fixtures keep time and failures explicit.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use tempo::{AgainCommand, Clock, Components, SubmitCommand, Tempo};
use tempo_chess::{Catalog, ImportRequest, demo_catalog};
use tempo_learning::LearningState;
use tempo_store::{AnswerResult, Commit, MemoryStore, Snapshot, Store, StoreError, Write};
use tempo_types::{Revision, StudyDay, StudyTime, TargetId};

#[derive(Clone)]
struct SharedStore(Rc<RefCell<MemoryStore>>);
impl Store for SharedStore {
    fn snapshot(&self) -> Result<Snapshot, StoreError> {
        self.0.borrow().snapshot()
    }
    fn commit(&mut self, commit: Commit) -> Result<Revision, StoreError> {
        self.0.borrow_mut().commit(commit)
    }
}
struct SharedClock(Rc<Cell<StudyTime>>);
impl Clock for SharedClock {
    fn now(&self) -> StudyTime {
        self.0.get()
    }
}
pub(super) struct Harness {
    pub(super) app: Tempo,
    pub(super) store: Rc<RefCell<MemoryStore>>,
    pub(super) clock: Rc<Cell<StudyTime>>,
}
pub(super) fn at(day: i32) -> StudyTime {
    StudyTime {
        day: StudyDay(day),
        unix_ms: day as u64 * 86_400_000,
    }
}
pub(super) fn build(catalog: Catalog, configure: impl FnOnce(&mut Components)) -> Harness {
    let mut components = Components::fixtures();
    let clock = Rc::new(Cell::new(at(10)));
    components.clock = Box::new(SharedClock(clock.clone()));
    configure(&mut components);
    let store = Rc::new(RefCell::new(
        MemoryStore::new(
            "test-dataset".into(),
            catalog,
            components.learning.settings(),
        )
        .unwrap(),
    ));
    let app = Tempo::new(Box::new(SharedStore(store.clone())), components).unwrap();
    Harness { app, store, clock }
}
pub(super) fn catalog(ids: &[&str]) -> Catalog {
    let mut result = demo_catalog();
    result.items.retain(|id, _| ids.contains(&id.0.as_str()));
    for line in &mut result.lines {
        line.targets.retain(|id| result.items.contains_key(id));
    }
    result.lines.retain(|line| !line.targets.is_empty());
    result
}
pub(super) fn one(id: &str) -> Harness {
    build(catalog(&[id]), |_| {})
}
pub(super) fn start(h: &mut Harness) -> AnswerResult {
    h.app.next_exercise().unwrap().expect("expected exercise")
}
pub(super) fn command(prior: &AnswerResult, id: &str, answer: &str) -> SubmitCommand {
    SubmitCommand {
        dataset_id: prior.attempt.dataset.clone(),
        attempt_id: prior.attempt.id.clone(),
        submission_id: id.into(),
        expected_revision: prior.attempt.revision,
        answer: answer.into(),
    }
}
pub(super) fn answer(h: &mut Harness, prior: &AnswerResult, id: &str, uci: &str) -> AnswerResult {
    h.app.submit(command(prior, id, uci)).unwrap()
}
pub(super) fn external(h: &Harness, target: &str, event: &str) -> AgainCommand {
    let snapshot = h.app.snapshot().unwrap();
    AgainCommand {
        dataset_id: snapshot.dataset,
        target: target.into(),
        provider: "fixture".into(),
        event_id: event.into(),
        observed_at_ms: 0,
        expected_content_version: snapshot.catalog.items[&TargetId::from(target)].version,
    }
}
pub(super) fn import_request(pgn: &str) -> ImportRequest {
    ImportRequest {
        pgn: pgn.into(),
        source: "integration-fixture".into(),
        repertoire: None,
        learner_color: None,
    }
}
pub(super) fn seed_due(h: &mut Harness, ids: &[&str]) {
    let snapshot = h.app.snapshot().unwrap();
    h.store
        .borrow_mut()
        .commit(Commit {
            expected_revision: snapshot.revision,
            expected_dataset: snapshot.dataset,
            writes: ids
                .iter()
                .map(|id| Write::Learning {
                    target: (*id).into(),
                    state: LearningState {
                        introduced: true,
                        due: Some(StudyDay(10)),
                        ..LearningState::default()
                    },
                })
                .collect(),
        })
        .unwrap();
}
