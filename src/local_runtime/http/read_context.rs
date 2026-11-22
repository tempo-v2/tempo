//! Read dataset context independently of the foreground owner.

use super::{Response, body};
use hyper::StatusCode;
use std::{
    io,
    path::Path,
    sync::mpsc::{SyncSender, sync_channel},
    thread,
    time::Instant,
};
use tempo_store::SqliteStore;
use tokio::{sync::oneshot, time::timeout_at};

#[derive(Clone)]
pub(super) struct ContextReader(SyncSender<oneshot::Sender<Response>>);

impl ContextReader {
    pub(super) fn start(database: &Path) -> io::Result<Self> {
        let database = database.to_owned();
        let (send, receive) = sync_channel::<oneshot::Sender<Response>>(1);
        thread::Builder::new()
            .name("tempo-context-read".into())
            .spawn(move || {
                let mut store = None;
                for respond in receive {
                    if respond.is_closed() {
                        continue;
                    }
                    if store.is_none() {
                        store = SqliteStore::open_read_only(&database).ok();
                    }
                    let reply = match store
                        .as_ref()
                        .and_then(|store| store.context_identity().ok())
                    {
                        Some((dataset, schema_version)) => body(
                            "application/json; charset=utf-8",
                            serde_json::json!({
                                "status": "ready", "dataset_id": dataset,
                                "schema_version": schema_version,
                            })
                            .to_string(),
                        ),
                        None => unavailable(),
                    };
                    let _ = respond.send(reply);
                }
            })?;
        Ok(Self(send))
    }

    pub(super) async fn read(self, deadline: Instant) -> Response {
        if Instant::now() >= deadline {
            return unavailable();
        }
        let (send, receive) = oneshot::channel();
        if self.0.try_send(send).is_err() {
            return unavailable();
        }
        match timeout_at(deadline.into(), receive).await {
            Ok(Ok(reply)) => reply,
            _ => unavailable(),
        }
    }
}

fn unavailable() -> Response {
    body(
        "application/json; charset=utf-8",
        r#"{"status":"unavailable","code":"storage_unavailable"}"#,
    )
    .with_status_code(StatusCode::SERVICE_UNAVAILABLE)
}
