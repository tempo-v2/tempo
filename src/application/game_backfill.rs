use super::Tempo;
use crate::AppError;
use crate::app_error::domain;
use crate::game_backfill::{BackfillJob, BackfillRequest};
use std::collections::{BTreeMap, BTreeSet};
use tempo_chess::ContentKind;
use tempo_types::DatasetId;

impl Tempo {
    pub(crate) fn game_now(&self) -> u64 {
        self.components.clock.now().unix_ms
    }

    pub(crate) fn read_backfill(
        &self,
        dataset: &DatasetId,
        id: Option<&str>,
    ) -> Result<Option<BackfillJob>, AppError> {
        self.store
            .game_backfill_state(dataset, id)?
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| domain("invalid_backfill", error))
    }

    pub(crate) fn historical_pgn_window(
        &self,
        dataset: &DatasetId,
        learner: &str,
        from: u64,
        to: u64,
    ) -> Result<Vec<tempo_store::GameRecord>, AppError> {
        Ok(self
            .store
            .historical_pgn_window(dataset, learner, from, to)?)
    }

    pub(crate) fn create_backfill(
        &mut self,
        request: BackfillRequest,
    ) -> Result<BackfillJob, AppError> {
        let snapshot = self.store.content_state()?;
        if snapshot.dataset != request.dataset_id {
            return Err(AppError::new(
                "stale_dataset",
                "The active dataset changed.",
            ));
        }
        let ids: BTreeSet<_> = snapshot
            .catalog
            .items
            .values()
            .filter(|item| item.active())
            .filter_map(|item| match &item.definition.kind {
                ContentKind::Repertoire { repertoire, .. }
                    if !snapshot.suspended_repertoires.contains(repertoire) =>
                {
                    Some(repertoire)
                }
                _ => None,
            })
            .collect();
        let mut resets = BTreeMap::new();
        for id in ids {
            resets.insert(
                id.clone(),
                self.store
                    .repertoire_reset_generation(&snapshot.dataset, id)?,
            );
        }
        let accounts_to_recognize: Vec<_> = [
            ("lichess", &request.lichess),
            ("chess_com", &request.chess_com),
        ]
        .into_iter()
        .filter_map(|(source, account)| {
            account
                .as_ref()
                .map(|account| (source.into(), account.clone()))
        })
        .collect();
        let job = BackfillJob::new(request, resets);
        self.save_backfill(&job, Some(&accounts_to_recognize))?;
        Ok(job)
    }

    pub(crate) fn save_backfill(
        &mut self,
        job: &BackfillJob,
        accounts_to_recognize: Option<&[(String, String)]>,
    ) -> Result<(), AppError> {
        let state =
            serde_json::to_string(job).map_err(|error| domain("invalid_backfill", error))?;
        self.store.save_game_backfill_state(
            &job.request.dataset_id,
            &job.id,
            &state,
            accounts_to_recognize,
        )?;
        Ok(())
    }
}
