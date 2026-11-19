//! Run local probability inference outside the HTTP and Study application loop.
use crate::{AppError, Tempo, app_error::domain};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, sync_channel},
    },
    time::Duration,
};
use tempo_chess::{AuthoredRepertoire, LearnerColor, PositionKey, legal_model_moves};
use tempo_store::{SqliteStore, Store};
use tempo_types::{DatasetId, Revision, TargetId};

pub(crate) const MODEL_KEY: &str = "maia2-0.11.0:65aae8465eed5e65df66a24ea7370715579f9e5435098d06fe18bdb1e267e997:rapid:1500:1500:cpu";
pub(crate) const PROFILE_LABEL: &str = "Maia Rapid 1500";
pub(crate) type Distribution = BTreeMap<String, f64>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequest {
    pub dataset_id: DatasetId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapRequest {
    pub dataset_id: DatasetId,
    pub repertoire_id: String,
    pub position: PositionKey,
    pub actual_move: String,
    pub document_revision: String,
    pub response: Option<String>,
}

pub(crate) struct ModelInput {
    pub dataset: DatasetId,
    pub revision: Revision,
    pub books: Vec<ModelBook>,
}

pub(crate) struct ModelBook {
    pub pgn: String,
    pub color: LearnerColor,
    pub targets: BTreeMap<PositionKey, TargetId>,
}

#[derive(Clone, Serialize)]
struct Status {
    status: &'static str,
    profile: &'static str,
    completed: usize,
    total: usize,
    projected: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            status: "idle",
            profile: PROFILE_LABEL,
            completed: 0,
            total: 0,
            projected: 0,
            error: None,
        }
    }
}

pub struct GameModel {
    database: PathBuf,
    status: Arc<Mutex<Status>>,
}

impl GameModel {
    pub fn new(database: &Path) -> Self {
        Self {
            database: database.to_path_buf(),
            status: Arc::new(Mutex::new(Status::default())),
        }
    }

    pub fn status(&self) -> Value {
        json!(*self.status.lock().expect("model status"))
    }

    pub fn start(&self, app: &Tempo, request: ModelRequest) -> Result<Value, AppError> {
        if self.status.lock().expect("model status").status == "running" {
            return Ok(self.status());
        }
        let input = app.model_input(&request.dataset_id)?;
        let directory = self
            .database
            .parent()
            .unwrap_or(Path::new("."))
            .join("maia");
        let status = Arc::clone(&self.status);
        let database = self.database.clone();
        *status.lock().expect("model status") = Status {
            status: "running",
            ..Status::default()
        };
        if let Err(error) = std::thread::Builder::new()
            .name("tempo-maia".into())
            .spawn(move || {
                if let Err(error) = run(&database, &directory, input, &status) {
                    let mut view = status.lock().expect("model status");
                    view.status = "incomplete";
                    view.error = Some(error.to_string());
                }
            })
        {
            *self.status.lock().expect("model status") = Status::default();
            return Err(domain("model_unavailable", error));
        }
        Ok(self.status())
    }
}

struct PreparedBook {
    book: ModelBook,
    tree: Prefix,
}

#[derive(Default)]
struct Prefix {
    position: Option<(PositionKey, LearnerColor)>,
    replies: BTreeMap<String, Prefix>,
}

impl Prefix {
    fn from_book(book: AuthoredRepertoire) -> Self {
        let mut root = Self::default();
        for path in book.routes {
            let mut node = &mut root;
            for (position, played) in path {
                let side = book.positions[&position].side;
                node.position = Some((position, side));
                node = node.replies.entry(played).or_default();
            }
        }
        root
    }

    fn project(
        &self,
        book: &ModelBook,
        probability: Option<f64>,
        cache: &BTreeMap<PositionKey, Distribution>,
        path_hits: &mut BTreeSet<TargetId>,
        reach: &mut BTreeMap<TargetId, Option<f64>>,
    ) {
        let Some((position, side)) = &self.position else {
            return;
        };
        let first_hit = book
            .targets
            .get(position)
            .filter(|target| path_hits.insert((*target).clone()));
        if let Some(target) = first_hit {
            let total = reach.entry(target.clone()).or_insert(Some(0.0));
            *total = total.zip(probability).map(|(sum, path)| sum + path);
        }
        let opponent = *side != book.color;
        for (played, next) in &self.replies {
            let child = if opponent {
                probability
                    .zip(cache.get(position).and_then(|moves| moves.get(played)))
                    .map(|(path, local)| path * local)
            } else {
                probability
            };
            next.project(book, child, cache, path_hits, reach);
        }
        if let Some(target) = first_hit {
            path_hits.remove(target);
        }
    }
}

fn run(
    database: &Path,
    directory: &Path,
    input: ModelInput,
    status: &Mutex<Status>,
) -> Result<(), AppError> {
    let mut store = SqliteStore::open(database)?;
    let mut jobs = BTreeSet::new();
    let mut books = Vec::new();
    for book in input.books {
        let authored = AuthoredRepertoire::parse(&book.pgn)
            .map_err(|error| domain("invalid_repertoire", error))?;
        jobs.extend(
            authored
                .positions
                .iter()
                .filter(|(_, node)| !node.replies.is_empty() && node.side != book.color)
                .map(|(position, _)| position.clone()),
        );
        books.push(PreparedBook {
            book,
            tree: Prefix::from_book(authored),
        });
    }
    status.lock().expect("model status").total = jobs.len();
    if books.is_empty() {
        return Err(AppError::new(
            "model_unavailable",
            "No verified repertoire documents are available.",
        ));
    }
    let mut cache = BTreeMap::new();
    let mut missing = Vec::new();
    for position in jobs {
        if let Some(distribution) = store.game_move_distribution(MODEL_KEY, &position.0)? {
            cache.insert(position, distribution);
        } else {
            missing.push(position);
        }
    }
    status.lock().expect("model status").completed = cache.len();
    let result = if missing.is_empty() {
        Ok(())
    } else {
        (|| {
            let mut maia = Maia::start(directory)?;
            for position in missing {
                let distribution = maia.infer(&position)?;
                store.save_game_move_distribution(MODEL_KEY, &position.0, &distribution)?;
                cache.insert(position, distribution);
                status.lock().expect("model status").completed = cache.len();
            }
            Ok(())
        })()
    };
    let mut reach = BTreeMap::new();
    for PreparedBook { book, tree } in books {
        tree.project(&book, Some(1.0), &cache, &mut BTreeSet::new(), &mut reach);
    }
    let reach: BTreeMap<_, _> = reach
        .into_iter()
        .filter_map(|(target, reach)| {
            reach.map(|mass| (target, (mass.clamp(0.0, 1.0) * 1_000_000.0).round() as u32))
        })
        .collect();
    store.replace_game_model_reach(&input.dataset, MODEL_KEY, input.revision, &reach)?;
    let mut view = status.lock().expect("model status");
    view.projected = reach.len();
    view.status = if result.is_ok() {
        "complete"
    } else {
        "incomplete"
    };
    view.error = result.err().map(|error: AppError| error.message);
    Ok(())
}

struct Maia {
    child: Child,
    input: ChildStdin,
    output: Receiver<String>,
}

impl Maia {
    fn start(directory: &Path) -> Result<Self, AppError> {
        let mut child = Command::new(directory.join("venv/bin/python"))
            .args(["-I", "-u", "-c", include_str!("../tools/maia/infer.py")])
            .arg(directory.join("weights"))
            .env("OMP_NUM_THREADS", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| domain("maia_runtime_unavailable", error))?;
        let input = child.stdin.take().expect("piped model input");
        let stdout = child.stdout.take().expect("piped model output");
        let (send, output) = sync_channel(1);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let process = Self {
            child,
            input,
            output,
        };
        let ready: Value = process.read(Duration::from_secs(30))?;
        if ready["ready"] != true {
            return Err(AppError::new(
                "maia_runtime_unavailable",
                "The Maia worker did not initialize.",
            ));
        }
        Ok(process)
    }

    fn read<T: serde::de::DeserializeOwned>(&self, timeout: Duration) -> Result<T, AppError> {
        let reply = self
            .output
            .recv_timeout(timeout)
            .map_err(|error| domain("maia_timeout", error))?;
        serde_json::from_str(&reply).map_err(|error| domain("invalid_maia_reply", error))
    }

    fn infer(&mut self, position: &PositionKey) -> Result<Distribution, AppError> {
        let fen = json!(format!("{} 0 1", position.0));
        writeln!(self.input, "{fen}").map_err(|error| domain("maia_request", error))?;
        let distribution: Distribution = self.read(Duration::from_secs(10))?;
        let legal =
            legal_model_moves(position).map_err(|error| domain("invalid_position", error))?;
        let mass: f64 = distribution.values().sum();
        if !distribution.keys().eq(legal.iter())
            || distribution.values().any(|p| !p.is_finite() || *p < 0.0)
            || (mass - 1.0).abs() > 1e-8
        {
            return Err(AppError::new(
                "invalid_maia_reply",
                "Expected one probability for every legal move.",
            ));
        }
        Ok(distribution)
    }
}

impl Drop for Maia {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempo_chess::{ContentKind, compile_repertoire};

    #[test]
    fn modeled_reach_counts_first_arrival_and_keeps_missing_paths_unknown() {
        let pgn = "1. Nf3 Nf6 (1... g6 2. g3 Nf6 3. Bg2) 2. g3 g6 3. Bg2 *";
        let color = LearnerColor::White;
        let catalog = compile_repertoire(pgn, "book", "Book", color, "test").unwrap();
        let targets = catalog
            .items
            .values()
            .filter_map(|item| match &item.definition.kind {
                ContentKind::Repertoire { position, .. } => {
                    Some((position.clone(), item.target.clone()))
                }
                _ => None,
            })
            .collect();
        let target = catalog
            .items
            .values()
            .find(|item| item.definition.decisions[0].expected.uci == "f1g2")
            .unwrap()
            .target
            .clone();
        let authored = AuthoredRepertoire::parse(pgn).unwrap();
        let paths = authored.routes.clone();
        let root = Prefix::from_book(authored);
        let book = ModelBook {
            pgn: pgn.into(),
            color,
            targets,
        };
        let branch = paths[0][1].0.clone();
        let mut cache = BTreeMap::from([(
            branch,
            BTreeMap::from([("g8f6".into(), 0.4), ("g7g6".into(), 0.3)]),
        )]);
        cache.insert(
            paths[0][3].0.clone(),
            BTreeMap::from([("g7g6".into(), 0.5)]),
        );
        let mut reach = BTreeMap::new();
        root.project(&book, Some(1.0), &cache, &mut BTreeSet::new(), &mut reach);
        assert_eq!(reach[&target], None);
        cache.insert(
            paths[1][3].0.clone(),
            BTreeMap::from([("g8f6".into(), 0.6)]),
        );
        let mut reach = BTreeMap::new();
        root.project(&book, Some(1.0), &cache, &mut BTreeSet::new(), &mut reach);
        assert!((reach[&target].unwrap() - (0.4 * 0.5 + 0.3 * 0.6)).abs() < 1e-12);
    }
}
