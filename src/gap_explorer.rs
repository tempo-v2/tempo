//! Resolve a game gap and deliver its rated Explorer evidence to Fix Response.
use crate::{
    AppError, Tempo,
    explorer::{ExplorerClient, Ratings},
    game_evidence::check_id,
    game_model::GapRequest,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapExplorerRequest {
    request_id: String,
    gap: GapRequest,
    ratings: Ratings,
}

pub struct GapExplorer {
    client: Arc<ExplorerClient>,
    reply: Arc<Mutex<Value>>,
}
impl GapExplorer {
    pub fn new(database: &Path) -> Self {
        Self::with_client(Arc::new(ExplorerClient::new(database)))
    }
    pub fn with_client(client: Arc<ExplorerClient>) -> Self {
        Self {
            client,
            reply: Arc::new(Mutex::new(json!({"status":"idle"}))),
        }
    }
    pub fn status(&self) -> Value {
        self.reply.lock().expect("Explorer reply").clone()
    }
    pub fn start(&self, app: &Tempo, request: GapExplorerRequest) -> Result<Value, AppError> {
        check_id(&request.request_id)?;
        if self.status()["status"] == "running" {
            return Err(AppError::new(
                "explorer_busy",
                "A Lichess lookup is still running.",
            ));
        }
        if self.client.limited() {
            return Err(AppError::new(
                "rate_limited",
                "Lichess is limiting requests. Try again in a minute.",
            ));
        }
        let (draft, revision) = app.gap_position(request.gap)?;
        let queued = json!({
            "status": "running", "request_id": request.request_id, "content_revision": revision,
        });
        let mut result = queued.clone();
        *self.reply.lock().expect("Explorer reply") = queued.clone();
        let reply = Arc::clone(&self.reply);
        let token_path = self.client.token_path.clone();
        let explorer = self.client.clone();
        if std::thread::Builder::new()
            .name("tempo-gap-explorer".into())
            .spawn(move || {
                match explorer.position(&draft.fen, &draft.legal_moves, Some(request.ratings)) {
                    Ok(stats) => {
                        result["status"] = json!("complete");
                        result["statistics"] = json!(stats);
                    }
                    Err(code) => {
                        result["status"] = json!("unavailable");
                        result["code"] = json!(code);
                        match code {
                            "token_missing" | "token_invalid" | "token_rejected" => {
                                result["token_path"] = json!(token_path.to_string_lossy());
                            }
                            _ => {}
                        }
                    }
                }
                *reply.lock().expect("Explorer reply") = result;
            })
            .is_err()
        {
            *self.reply.lock().expect("Explorer reply") = json!({"status": "idle"});
            return Err(AppError::new(
                "explorer_unavailable",
                "The Lichess lookup could not start.",
            ));
        }
        Ok(queued)
    }
}
