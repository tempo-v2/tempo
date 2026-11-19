//! Repertoire authoring preference. Inputs are validated moves and learner-relative engine scores.
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use tempo_chess::{NearbyPosition, NearbyRepertoirePositions};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RepertoireScope {
    Current,
    Other,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SimilarMove<'a> {
    pub uci: &'a str,
    pub position_count: usize,
}

#[derive(Default, Serialize)]
pub(crate) struct SimilarMoves<'a> {
    pub current: Vec<SimilarMove<'a>>,
    pub other: Vec<SimilarMove<'a>>,
}

impl<'a> SimilarMoves<'a> {
    pub fn rank(nearby: &'a NearbyRepertoirePositions) -> Self {
        Self {
            current: rank_group(&nearby.current),
            other: rank_group(&nearby.other),
        }
    }

    pub fn ordered(&self) -> impl Iterator<Item = (RepertoireScope, &SimilarMove<'a>)> {
        let mut seen = BTreeSet::new();
        self.current
            .iter()
            .map(|mv| (RepertoireScope::Current, mv))
            .chain(self.other.iter().map(|mv| (RepertoireScope::Other, mv)))
            .filter(move |(_, mv)| seen.insert(mv.uci))
    }
}

fn rank_group(nearby: &[NearbyPosition]) -> Vec<SimilarMove<'_>> {
    let mut counts = BTreeMap::new();
    for position in nearby {
        let unique = position
            .responses
            .iter()
            .filter(|response| response.reusable)
            .map(|response| response.uci.as_str())
            .collect::<BTreeSet<_>>();
        for uci in unique {
            let (count, closest) = counts.entry(uci).or_insert((0, position.distance));
            *count += 1;
            *closest = (*closest).min(position.distance);
        }
    }
    let mut ranked: Vec<_> = counts.into_iter().collect();
    ranked.sort_unstable_by_key(|(uci, (count, distance))| {
        (std::cmp::Reverse(*count), *distance, *uci)
    });
    ranked
        .into_iter()
        .map(|(uci, (position_count, _))| SimilarMove {
            uci,
            position_count,
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "basis", rename_all = "snake_case")]
pub(crate) enum Recommendation<'a> {
    Similar {
        uci: &'a str,
        scope: RepertoireScope,
        position_count: usize,
        loss_centipawns: u32,
    },
    Stockfish {
        uci: &'a str,
        nearest_loss_centipawns: u32,
    },
}

pub(crate) fn recommend<'a>(
    similar: &SimilarMoves<'a>,
    best: (&'a str, Option<i32>),
    scores: impl IntoIterator<Item = (&'a str, Option<i32>)>,
) -> Option<Recommendation<'a>> {
    let scores: BTreeMap<_, _> = scores.into_iter().collect();
    let mut smallest_loss = None;
    let mut incomplete = false;
    for (scope, candidate) in similar.ordered() {
        let loss = if candidate.uci == best.0 {
            Some(0)
        } else {
            best.1
                .zip(scores.get(candidate.uci).copied().flatten())
                .map(|(top, score)| top.saturating_sub(score).max(0) as u32)
        };
        let Some(loss) = loss else {
            incomplete = true;
            continue;
        };
        if loss <= 50 {
            return Some(Recommendation::Similar {
                uci: candidate.uci,
                scope,
                position_count: candidate.position_count,
                loss_centipawns: loss,
            });
        }
        smallest_loss = Some(smallest_loss.map_or(loss, |previous: u32| previous.min(loss)));
    }
    if !incomplete
        && let Some(loss) = smallest_loss
        && loss > 150
    {
        return Some(Recommendation::Stockfish {
            uci: best.0,
            nearest_loss_centipawns: loss,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempo_chess::NearbyResponse;

    fn position(distance: u32, moves: &[(&str, bool)]) -> NearbyPosition {
        NearbyPosition {
            fen: String::new(),
            distance,
            responses: moves
                .iter()
                .enumerate()
                .map(|(index, (uci, reusable))| NearbyResponse {
                    repertoire_id: index.to_string(),
                    uci: (*uci).to_owned(),
                    san: "Any display label".to_owned(),
                    reusable: *reusable,
                })
                .collect(),
        }
    }

    #[test]
    fn ranks_by_frequency_then_closeness_and_does_not_count_same_position_twice() {
        let nearby = NearbyRepertoirePositions {
            current: vec![
                position(2, &[("g1f3", true), ("g1f3", true), ("a2a3", false)]),
                position(3, &[("e4d5", true), ("d2d3", true)]),
                position(4, &[("e4d5", true)]),
            ],
            other: vec![position(1, &[("d2d4", true), ("e4d5", true)])],
        };
        let ranked = SimilarMoves::rank(&nearby);
        assert_eq!(
            ranked
                .ordered()
                .map(|(scope, mv)| (scope, mv.uci, mv.position_count))
                .collect::<Vec<_>>(),
            [
                (RepertoireScope::Current, "e4d5", 2),
                (RepertoireScope::Current, "g1f3", 1),
                (RepertoireScope::Current, "d2d3", 1),
                (RepertoireScope::Other, "d2d4", 1),
            ]
        );
        assert_eq!(
            recommend(
                &ranked,
                ("d2d4", Some(20)),
                [("e4d5", Some(-70)), ("g1f3", Some(-29))]
            ),
            Some(Recommendation::Similar {
                uci: "g1f3",
                scope: RepertoireScope::Current,
                position_count: 1,
                loss_centipawns: 49,
            }),
        );
        assert_eq!(
            recommend(
                &ranked,
                ("d2d4", Some(20)),
                [("e4d5", Some(-70)), ("g1f3", Some(-31))]
            ),
            Some(Recommendation::Similar {
                uci: "d2d4",
                scope: RepertoireScope::Other,
                position_count: 1,
                loss_centipawns: 0,
            }),
        );
    }

    #[test]
    fn thresholds_apply_to_the_closest_score_and_incomplete_scores_leave_fallback_blank() {
        let nearby = NearbyRepertoirePositions {
            current: vec![
                position(1, &[("e4d5", true)]),
                position(2, &[("g1f3", true)]),
            ],
            other: vec![],
        };
        let ranked = SimilarMoves::rank(&nearby);
        let decide = |loss| {
            recommend(
                &ranked,
                ("d2d4", Some(-10)),
                [("e4d5", Some(-230)), ("g1f3", Some(-10 - loss))],
            )
        };
        assert_eq!(
            decide(50),
            Some(Recommendation::Similar {
                uci: "g1f3",
                scope: RepertoireScope::Current,
                position_count: 1,
                loss_centipawns: 50,
            })
        );
        assert_eq!(decide(51), None);
        assert_eq!(decide(150), None);
        assert_eq!(
            decide(151),
            Some(Recommendation::Stockfish {
                uci: "d2d4",
                nearest_loss_centipawns: 151,
            })
        );
        assert_eq!(
            recommend(
                &ranked,
                ("d2d4", Some(20)),
                [("e4d5", Some(-150)), ("g1f3", None)]
            ),
            None
        );
        assert_eq!(
            recommend(&ranked, ("d2d4", None), [("e4d5", Some(-150))]),
            None
        );
        assert_eq!(
            recommend(&ranked, ("g1f3", None), [("e4d5", Some(-150))]),
            Some(Recommendation::Similar {
                uci: "g1f3",
                scope: RepertoireScope::Current,
                position_count: 1,
                loss_centipawns: 0,
            })
        );
        assert_eq!(
            recommend(&SimilarMoves::default(), ("d2d4", Some(20)), []),
            None
        );
    }
}
