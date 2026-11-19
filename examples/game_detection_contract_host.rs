//! External detector contract fixture using the same host as normal serve.
use std::{path::Path, time::Duration};
use tempo::{
    game_detection::{AnalysisOptions, DetectorRegistry},
    local_runtime::serve_with_detectors,
};
use tempo_chess::{
    DetectedMoment, DetectionControl, DetectionOutcome, DetectorDescriptor, GameDetectionInput,
    GameDetector,
};

struct Fixture {
    scenario: String,
    alternate: bool,
}

impl GameDetector for Fixture {
    fn descriptor(&self) -> DetectorDescriptor {
        DetectorDescriptor {
            id: if self.alternate {
                "contract-alternate"
            } else {
                "contract-fixture"
            }
            .into(),
            revision: if self.alternate { "2" } else { "1" }.into(),
            configuration_fingerprint: self.scenario.clone(),
            label: "External contract fixture".into(),
        }
    }

    fn detect(&self, input: &GameDetectionInput, stop: &dyn DetectionControl) -> DetectionOutcome {
        if let Err(reason) = stop.check() {
            return DetectionOutcome::Stopped(reason);
        }
        let timeline = input.timeline().occurrences();
        eprintln!(
            "Fixture input {}",
            serde_json::json!({
                "learner": input.learner(),
                "occurrence_count": timeline.len(),
                "start_fen": timeline.first().map(|board| board.fen()),
            })
        );
        if self.scenario == "delayed"
            && timeline
                .first()
                .is_some_and(|board| board.fen().ends_with(" 12"))
        {
            std::thread::sleep(Duration::from_millis(1200));
        }
        match self.scenario.as_str() {
            "empty" => DetectionOutcome::Complete(Vec::new()),
            "unsupported" => DetectionOutcome::Unsupported,
            "failed" => DetectionOutcome::Failed("fixture failure".into()),
            "slow" => {
                std::thread::sleep(Duration::from_millis(1000));
                DetectionOutcome::Complete(Vec::new())
            }
            _ => {
                let board = timeline.iter().find(|board| {
                    board.side_to_move() == input.learner() && board.next_move().is_some()
                });
                let Some(board) = board else {
                    return DetectionOutcome::Complete(Vec::new());
                };
                DetectionOutcome::Complete(vec![DetectedMoment {
                    occurrence: if self.scenario == "invalid" {
                        timeline.len()
                    } else {
                        board.offset()
                    },
                    code: "test-played-move".into(),
                    subjects: Vec::new(),
                    candidate_move: board.next_move().map(|answer| answer.uci.clone()),
                    explanation: "First played move by the selected learner.".into(),
                }])
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [database, ui, port, scenario] = args.as_slice() else {
        return Err("usage: game_detection_contract_host DATABASE UI_DIST PORT SCENARIO".into());
    };
    let mut registry = DetectorRegistry::default();
    registry.register(Fixture {
        scenario: scenario.clone(),
        alternate: false,
    })?;
    if scenario == "delayed" {
        registry.register(Fixture {
            scenario: "alternate".into(),
            alternate: true,
        })?;
    }
    let options = if scenario == "slow" {
        AnalysisOptions {
            deadline: Duration::from_millis(40),
            retained_results: 2,
        }
    } else {
        AnalysisOptions::default()
    };
    serve_with_detectors(
        Path::new(database),
        Path::new(ui),
        port.parse()?,
        registry,
        options,
    )
}
