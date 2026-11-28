//! Queue policy for selecting and mixing eligible daily work.

use std::collections::HashSet;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Review,
    Introduction,
}

impl Group {
    pub fn other(self) -> Self {
        match self {
            Self::Review => Self::Introduction,
            Self::Introduction => Self::Review,
        }
    }
}

/// Return the preferred group and the balance to save if its head is taken.
pub fn mix(day: &str, balance: i64, reviews: i64, introductions: i64) -> (Group, i64) {
    const UNIT: i64 = 1_000_000;
    if introductions == 0 {
        return (Group::Review, 0);
    }
    if reviews == 0 {
        return (Group::Introduction, 0);
    }

    // Use a different threshold for each remaining introduction count. Keep
    // the variation small enough to spread introductions between reviews.
    let mut hash = blake3::Hasher::new();
    hash.update(day.as_bytes());
    hash.update(&introductions.to_le_bytes());
    let bytes = hash.finalize();
    let sample = u32::from_le_bytes(bytes.as_bytes()[..4].try_into().unwrap());
    let threshold = 350_000 + i64::from(sample % 300_001);
    let total = reviews + introductions;
    let next = balance + (UNIT * introductions + total / 2) / total;
    if next >= threshold {
        (Group::Introduction, next - UNIT)
    } else {
        (Group::Review, next)
    }
}

/// Openings supplies alternative prerequisite routes. A standalone exercise has
/// no prerequisite, and unresolved content evidence cannot authorize admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prerequisites {
    NoneRequired,
    Routes(Vec<Vec<String>>),
    Unknown,
}

pub struct Candidate {
    pub card: String,
    pub prerequisites: Prerequisites,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Selection {
    pub card: String,
    /// Selected or pinned parents whose durable introductions still need to be
    /// accepted before this card can be issued.
    pub waiting_on: Vec<String>,
}

/// Pick the highest ordered feasible candidate repeatedly. All non-introduced
/// parents in the chosen route must already have a slot in this day's plan.
pub fn pick(
    ordered: &[Candidate],
    introduced: &HashSet<String>,
    pinned: &HashSet<String>,
    limit: usize,
) -> Vec<Selection> {
    pick_until(ordered, introduced, pinned, limit, || false).expect("unbounded planning")
}

pub fn pick_until(
    ordered: &[Candidate],
    introduced: &HashSet<String>,
    pinned: &HashSet<String>,
    limit: usize,
    expired: impl Fn() -> bool,
) -> Option<Vec<Selection>> {
    let mut allocated: HashSet<&str> = pinned.iter().map(String::as_str).collect();
    let mut selected = Vec::new();
    while selected.len() < limit {
        let mut next = None;
        for candidate in ordered {
            if expired() {
                return None;
            }
            if introduced.contains(&candidate.card) || allocated.contains(candidate.card.as_str()) {
                continue;
            }
            let waiting_on = match &candidate.prerequisites {
                Prerequisites::NoneRequired => Vec::new(),
                Prerequisites::Routes(routes) => {
                    let mut best: Option<Vec<&String>> = None;
                    for route in routes {
                        if expired() {
                            return None;
                        }
                        let waiting = route
                            .iter()
                            .filter(|parent| !introduced.contains(*parent))
                            .map(|parent| allocated.contains(parent.as_str()).then_some(parent))
                            .collect::<Option<Vec<_>>>();
                        if let Some(waiting) = waiting
                            && best
                                .as_ref()
                                .is_none_or(|old| (waiting.len(), &waiting) < (old.len(), old))
                        {
                            best = Some(waiting);
                        }
                    }
                    let Some(best) = best else { continue };
                    best.into_iter().cloned().collect()
                }
                Prerequisites::Unknown => continue,
            };
            next = Some((candidate.card.as_str(), waiting_on));
            break;
        }
        let Some((card, waiting_on)) = next else {
            break;
        };
        allocated.insert(card);
        selected.push(Selection {
            card: card.to_owned(),
            waiting_on,
        });
    }
    Some(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, parents: &[&[&str]]) -> Candidate {
        Candidate {
            card: id.to_owned(),
            prerequisites: if parents.is_empty() {
                Prerequisites::NoneRequired
            } else {
                Prerequisites::Routes(
                    parents
                        .iter()
                        .map(|route| route.iter().map(|parent| (*parent).to_owned()).collect())
                        .collect(),
                )
            },
        }
    }

    #[test]
    fn new_parents_count_against_the_cap_and_are_issued_first() {
        let ordered = [
            card("descendant", &[&["parent"]]),
            card("parent", &[]),
            card("standalone", &[]),
        ];
        let introduced = HashSet::new();
        let pinned = HashSet::new();
        assert_eq!(
            pick(&ordered, &introduced, &pinned, 2),
            [
                Selection {
                    card: "parent".to_owned(),
                    waiting_on: vec![],
                },
                Selection {
                    card: "descendant".to_owned(),
                    waiting_on: vec!["parent".to_owned()],
                },
            ]
        );
        assert_eq!(pick(&ordered, &introduced, &pinned, 1).len(), 1);
    }

    #[test]
    fn alternate_introduced_route_is_ready_and_unresolved_never_enters() {
        let ordered = [
            Candidate {
                card: "unresolved".into(),
                prerequisites: Prerequisites::Unknown,
            },
            card("target", &[&["pinned"], &["known"], &["absent"]]),
            card("pinned", &[]),
        ];
        let introduced = HashSet::from(["known".to_owned()]);
        let pinned = HashSet::from(["pinned".to_owned()]);
        assert_eq!(
            pick(&ordered, &introduced, &pinned, 2),
            [Selection {
                card: "target".to_owned(),
                waiting_on: vec![],
            }]
        );
        assert_eq!(
            pick(&ordered, &HashSet::new(), &pinned, 2)[0].waiting_on,
            ["pinned"]
        );
    }
}
