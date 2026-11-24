//! Apply one bounded proposal per turn through the existing Store writer.

use tempo_queue::PreparedBatch;
use tempo_store::{
    PLAN_ENTRY_CHUNK, PlanPublishResult, PlanReclaimResult, PlanStageResult, SqliteStore,
    StoreError,
};
use tempo_types::{DatasetId, PlanBasis, PlanEntry, PlanGeneration, StudyDay};

const FOREGROUND_TURNS: u8 = 4;

/// The foreground queue retains its existing admission and claim rules.
#[derive(Default)]
struct WriteTurns {
    foreground: u8,
}

impl WriteTurns {
    fn apply_before_foreground(&self, apply_ready: bool) -> bool {
        apply_ready && self.foreground >= FOREGROUND_TURNS
    }

    fn foreground_turn(&mut self) {
        self.foreground = self.foreground.saturating_add(1).min(FOREGROUND_TURNS);
    }

    fn apply_turn(&mut self) {
        self.foreground = 0;
    }
}

struct CapturedBuild {
    basis: PlanBasis,
    entries: Vec<PlanEntry>,
}

struct OwnedStage {
    generation: PlanGeneration,
    build: CapturedBuild,
    cursor: usize,
}

type GenerationAddress = (DatasetId, PlanGeneration);

/// Own at most one staged generation, one superseding build and one reclaim
/// address. Plans are computed outside this owner.
#[derive(Default)]
struct PlanApply {
    newest_build: Option<u64>,
    pending: Option<CapturedBuild>,
    stage: Option<OwnedStage>,
    reclaim: Option<GenerationAddress>,
    last_head: Option<(PlanBasis, PlanGeneration)>,
}

#[derive(Debug, PartialEq, Eq)]
enum Applied {
    Idle,
    Begun,
    Staged,
    Published,
    Abandoned,
    Reclaiming,
    Reclaimed,
}

impl PlanApply {
    /// The capture coordinator attaches an increasing delivery order. This
    /// chooses which result to keep but never replaces its captured basis.
    fn offer(&mut self, order: u64, batch: PreparedBatch) {
        if self.newest_build.is_some_and(|newest| newest >= order) {
            return;
        }
        self.newest_build = Some(order);
        let (basis, entries) = batch.into_parts();
        self.pending = Some(CapturedBuild { basis, entries });
    }

    fn is_ready(&self) -> bool {
        self.pending.is_some() || self.stage.is_some() || self.reclaim.is_some()
    }

    /// The composition root lends its existing writer for exactly one turn.
    /// SQL errors leave ownership with this state machine for a later retry.
    fn apply_next(
        &mut self,
        store: &mut SqliteStore,
        today: StudyDay,
    ) -> Result<Applied, StoreError> {
        if let Some((dataset, generation)) = &self.reclaim {
            return match store.reclaim_study_plan(dataset, *generation)? {
                PlanReclaimResult::Pending => Ok(Applied::Reclaiming),
                PlanReclaimResult::Reclaimed
                | PlanReclaimResult::Missing
                | PlanReclaimResult::Protected => {
                    self.reclaim = None;
                    Ok(Applied::Reclaimed)
                }
            };
        }
        if let Some(stage) = self.stage.as_ref() {
            if stage.build.basis.day != today
                || self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.basis != stage.build.basis)
            {
                return Ok(self.abandon());
            }
            // A duplicate build for the same exact basis cannot replace the
            // already owned stage or its durable cursor.
            self.pending = None;
            return self.apply_stage(store, today);
        }
        let Some(pending) = self.pending.as_ref() else {
            return Ok(Applied::Idle);
        };
        if pending.basis.day != today {
            self.pending = None;
            return Ok(Applied::Abandoned);
        }
        if self
            .last_head
            .as_ref()
            .is_some_and(|(basis, _)| basis == &pending.basis)
        {
            self.pending = None;
            return Ok(Applied::Idle);
        }
        let expected = pending.entries.len() as u64;
        let generation = store.begin_study_plan(&pending.basis, expected)?;
        let build = self
            .pending
            .take()
            .expect("pending build was retained during begin");
        let Some(generation) = generation else {
            return Ok(Applied::Abandoned);
        };
        self.stage = Some(OwnedStage {
            generation,
            build,
            cursor: 0,
        });
        Ok(Applied::Begun)
    }

    fn apply_stage(
        &mut self,
        store: &mut SqliteStore,
        today: StudyDay,
    ) -> Result<Applied, StoreError> {
        let stage = self
            .stage
            .as_mut()
            .expect("owned stage reserved for this turn");
        let basis = &stage.build.basis;
        let stage_end = (stage.cursor + PLAN_ENTRY_CHUNK).min(stage.build.entries.len());
        if stage_end > stage.cursor {
            let chunk = StageChunk::new(
                basis,
                stage.cursor as u64,
                &stage.build.entries[stage.cursor..stage_end],
            )?;
            return match store.stage_study_plan(
                stage.generation,
                chunk.basis,
                chunk.start,
                chunk.entries,
            )? {
                PlanStageResult::Staged { next_ordinal }
                | PlanStageResult::CursorMismatch { next_ordinal }
                    if next_ordinal == chunk.through =>
                {
                    stage.cursor = stage_end;
                    Ok(Applied::Staged)
                }
                PlanStageResult::Stale
                | PlanStageResult::CursorMismatch { .. }
                | PlanStageResult::Staged { .. } => Ok(self.abandon()),
            };
        }
        match store.publish_study_plan(stage.generation, basis, today)? {
            PlanPublishResult::Published | PlanPublishResult::AlreadyPublished => {
                let stage = self.stage.take().expect("successful stage");
                self.reclaim = self
                    .last_head
                    .replace((stage.build.basis, stage.generation))
                    .map(|(basis, generation)| (basis.dataset, generation));
                Ok(Applied::Published)
            }
            PlanPublishResult::Incomplete | PlanPublishResult::Stale => Ok(self.abandon()),
        }
    }

    fn abandon(&mut self) -> Applied {
        let stage = self
            .stage
            .take()
            .expect("only owned stages can be abandoned");
        self.reclaim = Some((stage.build.basis.dataset, stage.generation));
        Applied::Abandoned
    }
}

/// Validate before the writer calls Store. Store repeats these checks and
/// compares against the durable generation cursor and original basis.
struct StageChunk<'a> {
    basis: &'a PlanBasis,
    start: u64,
    through: u64,
    entries: &'a [PlanEntry],
}

impl<'a> StageChunk<'a> {
    fn new(basis: &'a PlanBasis, start: u64, entries: &'a [PlanEntry]) -> Result<Self, StoreError> {
        if entries.is_empty() || entries.len() > PLAN_ENTRY_CHUNK {
            return Err(StoreError::Invalid(
                "prepared Stage must contain 1 to 127 entries".into(),
            ));
        }
        let mut through = start;
        for entry in entries {
            if entry.work.day != basis.day || entry.ordinal != through {
                return Err(StoreError::Invalid(
                    "prepared Stage must be consecutive on its captured day".into(),
                ));
            }
            through = through
                .checked_add(1)
                .ok_or_else(|| StoreError::Invalid("prepared Stage ordinal overflow".into()))?;
        }
        Ok(Self {
            basis,
            start,
            through,
            entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempo_chess::Catalog;
    use tempo_learning::{DailyLearning, LearningPolicy};
    use tempo_queue::{
        BatchPlanInput, BatchTarget, BatchWork, TeachAdmission, TeachKind, TeachingRemaining,
        build_batch_plan,
    };
    use tempo_store::StudyProfilePlanEffect;
    use tempo_types::{ContentVersion, MissRevision};

    const TODAY: StudyDay = StudyDay(10);
    const ZONE: &str = "America/New_York";
    const PROFILE: &str = r#"{"study_timezone":"America/New_York"}"#;
    const CHANGED_PROFILE: &str = r#"{"study_timezone":"America/New_York","intro_fixture":1}"#;

    fn fixture() -> (tempfile::TempDir, SqliteStore, PlanBasis) {
        let directory = tempfile::tempdir().unwrap();
        let dataset = DatasetId::from("prepared");
        let mut store = SqliteStore::create_with_profile(
            directory.path().join("study.sqlite"),
            dataset.clone(),
            Catalog::default(),
            DailyLearning::default().settings(),
            Some(PROFILE),
        )
        .unwrap();
        let basis = store
            .observe_study_calendar_day(&dataset, ZONE, TODAY)
            .unwrap()
            .unwrap()
            .plan;
        (directory, store, basis)
    }

    fn batch(basis: &PlanBasis, count: usize) -> PreparedBatch {
        let targets = (0..count)
            .map(|index| BatchTarget {
                target: tempo_types::TargetId(format!("target-{index:04}")),
                version: ContentVersion(1),
                available: true,
                teaching: TeachAdmission::Repertoire { at_frontier: true },
                work: Some(BatchWork::Teach {
                    revision: MissRevision(0),
                    kind: TeachKind::Introduction,
                }),
            })
            .collect::<Vec<_>>();
        build_batch_plan(BatchPlanInput {
            basis,
            expected_targets: count,
            targets: &targets,
            remaining: TeachingRemaining {
                repertoire: count,
                tactics: 0,
            },
            deferrals: &[],
            issued: None,
        })
        .unwrap()
    }

    #[test]
    fn a_continuous_foreground_queue_gives_each_real_stage_one_bounded_turn() {
        let (_directory, mut store, basis) = fixture();
        let mut turns = WriteTurns::default();
        let mut apply = PlanApply::default();
        apply.offer(1, batch(&basis, PLAN_ENTRY_CHUNK + 3));

        let mut sequence = Vec::new();
        let mut durable_cursors = Vec::new();
        for _ in 0..20 {
            if turns.apply_before_foreground(apply.is_ready()) {
                let result = apply.apply_next(&mut store, TODAY).unwrap();
                if result == Applied::Staged {
                    let stage = apply.stage.as_ref().unwrap();
                    let chunk = &stage.build.entries[..1];
                    durable_cursors.push(stage.cursor);
                    assert_eq!(
                        store
                            .stage_study_plan(stage.generation, &basis, 0, chunk)
                            .unwrap(),
                        PlanStageResult::CursorMismatch {
                            next_ordinal: stage.cursor as u64
                        },
                    );
                }
                sequence.push(result);
                turns.apply_turn();
            } else {
                turns.foreground_turn();
            }
        }
        assert_eq!(
            sequence,
            [
                Applied::Begun,
                Applied::Staged,
                Applied::Staged,
                Applied::Published
            ]
        );
        assert_eq!(durable_cursors, [PLAN_ENTRY_CHUNK, PLAN_ENTRY_CHUNK + 3]);
        assert!(!apply.is_ready());
        assert_eq!(
            store
                .study_plan_head(&basis.dataset)
                .unwrap()
                .unwrap()
                .basis,
            basis
        );
    }

    #[test]
    fn superseded_partial_stage_is_reclaimed_and_only_the_latest_result_publishes() {
        let (_directory, mut store, first) = fixture();
        let mut apply = PlanApply::default();
        apply.offer(1, batch(&first, PLAN_ENTRY_CHUNK + 3));
        assert_eq!(apply.apply_next(&mut store, TODAY).unwrap(), Applied::Begun);
        let superseded = apply.stage.as_ref().unwrap().generation;
        assert_eq!(
            apply.apply_next(&mut store, TODAY).unwrap(),
            Applied::Staged
        );
        assert!(store.study_plan_head(&first.dataset).unwrap().is_none());

        store
            .update_study_profile_for_plan(
                &first.dataset,
                PROFILE,
                CHANGED_PROFILE,
                StudyProfilePlanEffect::InvalidateMembership,
            )
            .unwrap();
        let latest = store
            .study_projection_basis(&first.dataset)
            .unwrap()
            .unwrap()
            .plan;
        apply.offer(3, batch(&latest, 0));
        apply.offer(2, batch(&first, 4));
        assert_eq!(
            apply.apply_next(&mut store, TODAY).unwrap(),
            Applied::Abandoned
        );
        assert_eq!(
            apply.apply_next(&mut store, TODAY).unwrap(),
            Applied::Reclaiming
        );
        assert_eq!(
            apply.apply_next(&mut store, TODAY).unwrap(),
            Applied::Reclaimed
        );
        assert_eq!(
            store
                .reclaim_study_plan(&first.dataset, superseded)
                .unwrap(),
            PlanReclaimResult::Missing,
        );
        assert_eq!(apply.apply_next(&mut store, TODAY).unwrap(), Applied::Begun);
        assert_eq!(
            apply.apply_next(&mut store, TODAY).unwrap(),
            Applied::Published
        );
        assert_eq!(
            store
                .study_plan_head(&first.dataset)
                .unwrap()
                .unwrap()
                .basis,
            latest
        );
    }

    #[test]
    fn a_fenced_stage_cannot_publish_and_a_new_day_restarts_the_cursor() {
        let (_directory, mut store, first) = fixture();
        let mut apply = PlanApply::default();
        apply.offer(1, batch(&first, 1));
        assert_eq!(apply.apply_next(&mut store, TODAY).unwrap(), Applied::Begun);

        let next_day = StudyDay(TODAY.0 + 1);
        let newer = store
            .observe_study_calendar_day(&first.dataset, ZONE, next_day)
            .unwrap()
            .unwrap()
            .plan;
        // The caller still using yesterday cannot bless the changed Store basis.
        assert_eq!(
            apply.apply_next(&mut store, TODAY).unwrap(),
            Applied::Abandoned
        );
        assert_eq!(
            apply.apply_next(&mut store, next_day).unwrap(),
            Applied::Reclaimed
        );
        assert!(store.study_plan_head(&first.dataset).unwrap().is_none());
        apply.offer(2, batch(&newer, 1));
        assert_eq!(
            apply.apply_next(&mut store, next_day).unwrap(),
            Applied::Begun
        );
        assert_eq!(apply.stage.as_ref().unwrap().cursor, 0);
        assert_eq!(
            apply.apply_next(&mut store, next_day).unwrap(),
            Applied::Staged
        );
        assert_eq!(apply.stage.as_ref().unwrap().cursor, 1);
        assert_eq!(
            apply.apply_next(&mut store, next_day).unwrap(),
            Applied::Published
        );
    }

    #[test]
    fn stage_admission_rejects_empty_over_budget_nonconsecutive_and_wrong_day() {
        let (_directory, _store, basis) = fixture();
        let (_, entries) = batch(&basis, PLAN_ENTRY_CHUNK + 1).into_parts();
        assert!(StageChunk::new(&basis, 0, &[]).is_err());
        assert!(StageChunk::new(&basis, 0, &entries).is_err());
        assert!(StageChunk::new(&basis, 0, &entries[1..2]).is_err());
        let mut wrong_day = entries[0].clone();
        wrong_day.work.day = StudyDay(TODAY.0 + 1);
        assert!(StageChunk::new(&basis, 0, &[wrong_day]).is_err());
        assert_eq!(
            StageChunk::new(&basis, 1, &entries[1..]).unwrap().through,
            (PLAN_ENTRY_CHUNK + 1) as u64
        );
    }
}
