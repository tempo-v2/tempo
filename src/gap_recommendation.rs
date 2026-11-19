//! Repertoire authoring preference. Inputs are validated moves and learner-relative engine scores.
use serde::Serialize;
use std::cmp::Reverse;
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
    #[serde(skip)]
    closest_distance: u32,
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

    fn break_ties(&mut self, loss: impl Fn(&'a str) -> Option<u32>) {
        for group in [&mut self.current, &mut self.other] {
            group.sort_unstable_by_key(|mv| {
                (
                    Reverse(mv.position_count),
                    loss(mv.uci).unwrap_or(u32::MAX),
                    mv.closest_distance,
                    mv.uci,
                )
            });
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
    ranked.sort_unstable_by_key(|(uci, (count, distance))| (Reverse(*count), *distance, *uci));
    ranked
        .into_iter()
        .map(|(uci, (position_count, closest_distance))| SimilarMove {
            uci,
            position_count,
            closest_distance,
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

pub(crate) fn rank_and_recommend<'a>(
    similar: &mut SimilarMoves<'a>,
    best: (&'a str, Option<i32>),
    scores: impl IntoIterator<Item = (&'a str, Option<i32>)>,
) -> Option<Recommendation<'a>> {
    let scores: BTreeMap<_, _> = scores.into_iter().collect();
    let loss = |uci| {
        if uci == best.0 {
            Some(0)
        } else {
            best.1
                .zip(scores.get(uci).copied().flatten())
                .map(|(top, score)| top.saturating_sub(score).max(0) as u32)
        }
    };
    similar.break_ties(loss);
    let mut smallest_loss = None;
    let mut incomplete = false;
    for (scope, candidate) in similar.ordered() {
        let loss = loss(candidate.uci);
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
        let mut ranked = SimilarMoves::rank(&nearby);
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
            rank_and_recommend(
                &mut ranked,
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
            rank_and_recommend(
                &mut ranked,
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
    fn stockfish_breaks_support_ties_before_distance() {
        let mut nearby = NearbyRepertoirePositions {
            current: vec![
                position(1, &[("f1e2", true)]),
                position(2, &[("h2h3", true)]),
                position(3, &[("c2c4", true)]),
                position(4, &[("h2h4", true)]),
            ],
            other: vec![position(0, &[("c2c4", true)])],
        };
        let scores = [
            ("f1e2", Some(117)),
            ("h2h3", Some(123)),
            ("h2h4", Some(118)),
        ];
        let mut ranked = SimilarMoves::rank(&nearby);
        assert_eq!(
            rank_and_recommend(&mut ranked, ("c2c4", Some(138)), scores),
            Some(Recommendation::Similar {
                uci: "c2c4",
                scope: RepertoireScope::Current,
                position_count: 1,
                loss_centipawns: 0,
            }),
        );
        assert_eq!(
            ranked.current.iter().map(|mv| mv.uci).collect::<Vec<_>>(),
            ["c2c4", "h2h3", "h2h4", "f1e2"]
        );

        nearby.current.push(position(5, &[("f1e2", true)]));
        let mut ranked = SimilarMoves::rank(&nearby);
        assert_eq!(
            rank_and_recommend(&mut ranked, ("c2c4", Some(138)), scores),
            Some(Recommendation::Similar {
                uci: "f1e2",
                scope: RepertoireScope::Current,
                position_count: 2,
                loss_centipawns: 21,
            }),
        );
    }

    #[test]
    fn equal_scores_and_unscored_moves_keep_structural_order() {
        let nearby = NearbyRepertoirePositions {
            current: vec![
                position(1, &[("f1e2", true)]),
                position(2, &[("h2h3", true)]),
                position(3, &[("c2c4", true)]),
                position(4, &[("h2h4", true)]),
            ],
            other: vec![],
        };
        let mut ranked = SimilarMoves::rank(&nearby);
        let result = rank_and_recommend(
            &mut ranked,
            ("d2d4", Some(138)),
            [("h2h3", Some(123)), ("c2c4", Some(123))],
        );
        assert!(matches!(
            result,
            Some(Recommendation::Similar { uci: "h2h3", .. })
        ));
        assert_eq!(
            ranked.current.iter().map(|mv| mv.uci).collect::<Vec<_>>(),
            ["h2h3", "c2c4", "f1e2", "h2h4"]
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
        let mut ranked = SimilarMoves::rank(&nearby);
        let mut decide = |loss| {
            rank_and_recommend(
                &mut ranked,
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
            rank_and_recommend(
                &mut ranked,
                ("d2d4", Some(20)),
                [("e4d5", Some(-150)), ("g1f3", None)]
            ),
            None
        );
        assert_eq!(
            rank_and_recommend(&mut ranked, ("d2d4", None), [("e4d5", Some(-150))]),
            None
        );
        assert_eq!(
            rank_and_recommend(&mut ranked, ("g1f3", None), [("e4d5", Some(-150))]),
            Some(Recommendation::Similar {
                uci: "g1f3",
                scope: RepertoireScope::Current,
                position_count: 1,
                loss_centipawns: 0,
            })
        );
        assert_eq!(
            rank_and_recommend(&mut SimilarMoves::default(), ("d2d4", Some(20)), []),
            None
        );
    }
}
