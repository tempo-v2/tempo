//! Test-owned clock and injected write failure around the real Daily Study and SQLite owners.
use std::{cell::Cell, collections::BTreeMap, rc::Rc};
use tempo::{Clock, CounterIds, DailyComponents, IntroductionLimits, SubmitCommand, Tempo};
use tempo_chess::{Catalog, LegalSequenceGrader, demo_catalog};
use tempo_learning::{DailyLearning, LearningPolicy, LearningState};
use tempo_store::{
    AnswerResult, Commit, DatasetIdentity, GameTargetRelevance, PortableGameEvidence, Snapshot,
    SqliteStore, Store, StoreError, StudyOrderingWrite, StudyScope, StudyState, TacticalRequest,
    Write,
};
use tempo_types::{AttemptId, DatasetId, Revision, StudyDay, StudyTime, TargetId};

struct FailOnceStore {
    store: SqliteStore,
    fail: Rc<Cell<bool>>,
}

impl DatasetIdentity for FailOnceStore {
    fn dataset_id(&self) -> Result<DatasetId, StoreError> {
        self.store.dataset_id()
    }
}

impl tempo_store::AddressedResume for FailOnceStore {
    fn resume_state(&self, id: &AttemptId) -> Result<tempo_store::ResumeState, StoreError> {
        self.store.resume_state(id)
    }
}

impl tempo_store::StudyContentRead for FailOnceStore {
    fn study_content_facts(
        &self,
        dataset: &DatasetId,
        target: &TargetId,
    ) -> Result<Option<tempo_store::StudyContentFacts>, StoreError> {
        self.store.study_content_facts(dataset, target)
    }

    fn current_study_definition(
        &self,
        dataset: &DatasetId,
        target: &TargetId,
        version: tempo_types::ContentVersion,
        lesson: tempo_store::StudyLessonRead,
    ) -> Result<Option<tempo_store::CurrentStudyDefinition>, StoreError> {
        self.store
            .current_study_definition(dataset, target, version, lesson)
    }

    fn issued_study_definition(
        &self,
        dataset: &DatasetId,
        target: &TargetId,
        version: tempo_types::ContentVersion,
        lesson: tempo_store::StudyLessonRead,
    ) -> Result<Option<tempo_store::IssuedStudyDefinition>, StoreError> {
        self.store
            .issued_study_definition(dataset, target, version, lesson)
    }
}

impl tempo_store::StudyReceiptRead for FailOnceStore {
    fn study_receipt(
        &self,
        dataset: &DatasetId,
        id: &tempo_types::SubmissionId,
    ) -> Result<tempo_store::StudyReceiptLookup, StoreError> {
        self.store.study_receipt(dataset, id)
    }
}

impl Store for FailOnceStore {
    fn snapshot(&self) -> Result<Snapshot, StoreError> {
        self.store.snapshot()
    }

    fn study_state(&self, scope: StudyScope<'_>) -> Result<StudyState, StoreError> {
        self.store.study_state(scope)
    }

    fn game_relevance(
        &self,
        dataset: &DatasetId,
        model: &str,
    ) -> Result<BTreeMap<TargetId, GameTargetRelevance>, StoreError> {
        self.store.game_relevance(dataset, model)
    }

    fn tactics_extra(&self, dataset: &DatasetId, day: StudyDay) -> Result<usize, StoreError> {
        self.store.tactics_extra(dataset, day)
    }

    fn regular_review_owed(&self, dataset: &DatasetId) -> Result<bool, StoreError> {
        self.store.regular_review_owed(dataset)
    }

    fn pending_tactical_requests(
        &self,
        dataset: &DatasetId,
    ) -> Result<Vec<TacticalRequest>, StoreError> {
        self.store.pending_tactical_requests(dataset)
    }

    fn commit_daily_issuance(
        &mut self,
        commit: Commit,
        scope: StudyScope<'_>,
        immediate_request: Option<u64>,
        ordering: StudyOrderingWrite,
    ) -> Result<Revision, StoreError> {
        self.store
            .commit_daily_issuance(commit, scope, immediate_request, ordering)
    }

    fn commit_study(
        &mut self,
        commit: Commit,
        scope: StudyScope<'_>,
    ) -> Result<Revision, StoreError> {
        if self.fail.replace(false) {
            return Err(StoreError::Unavailable(
                "injected test write failure".into(),
            ));
        }
        self.store.commit_study(commit, scope)
    }

    fn portable_state(&self) -> Result<(Snapshot, PortableGameEvidence), StoreError> {
        self.store.portable_state()
    }

    fn commit(&mut self, commit: Commit) -> Result<Revision, StoreError> {
        self.store.commit(commit)
    }
}

struct StudyClock(StudyTime);
impl Clock for StudyClock {
    fn now(&self) -> StudyTime {
        self.0
    }
}

pub(super) fn at(day: i32) -> StudyTime {
    StudyTime {
        day: StudyDay(day),
        unix_ms: day as u64 * 86_400_000,
    }
}

pub(super) fn daily_components(day: i32, next_id: u64) -> DailyComponents {
    DailyComponents {
        clock: Box::new(StudyClock(at(day))),
        grader: Box::new(LegalSequenceGrader),
        ids: Box::new(CounterIds(next_id)),
        learning: Box::new(DailyLearning::default()),
    }
}

pub(super) fn open(store: Box<dyn Store>, day: i32, next_id: u64) -> Tempo {
    Tempo::daily(
        store,
        daily_components(day, next_id),
        IntroductionLimits::default(),
        [42; 32],
        10,
    )
    .unwrap()
}

pub(super) struct Harness {
    pub(super) app: Tempo,
    pub(super) fail_next_commit: Rc<Cell<bool>>,
    _directory: tempfile::TempDir,
}

pub(super) fn empty_store(directory: &tempfile::TempDir) -> SqliteStore {
    SqliteStore::create(
        directory.path().join("empty.sqlite"),
        "empty-destination".into(),
        Catalog::default(),
        DailyLearning::default().settings(),
    )
    .unwrap()
}

pub(super) fn one(id: &str) -> Harness {
    let mut catalog: Catalog = demo_catalog();
    catalog.items.retain(|target, _| target.0 == id);
    for line in &mut catalog.lines {
        line.targets
            .retain(|target| catalog.items.contains_key(target));
    }
    catalog.lines.retain(|line| !line.targets.is_empty());
    let directory = tempfile::tempdir().unwrap();
    let mut store = SqliteStore::create(
        directory.path().join("study.sqlite"),
        "test-dataset".into(),
        catalog,
        DailyLearning::default().settings(),
    )
    .unwrap();
    if id == "r1:p1" {
        let state = store.snapshot().unwrap();
        store
            .commit(Commit {
                expected_dataset: state.dataset,
                expected_revision: state.revision,
                writes: vec![Write::Learning {
                    target: id.into(),
                    state: DailyLearning::teach(&LearningState::default(), at(9)).unwrap(),
                }],
            })
            .unwrap();
    }
    let fail_next_commit = Rc::new(Cell::new(false));
    let app = open(
        Box::new(FailOnceStore {
            store,
            fail: fail_next_commit.clone(),
        }),
        10,
        0,
    );
    Harness {
        app,
        fail_next_commit,
        _directory: directory,
    }
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
