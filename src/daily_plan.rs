//! Queue policy for choosing new work from an ordered daily candidate list.

use std::collections::HashSet;

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
    let mut allocated: HashSet<&str> = pinned.iter().map(String::as_str).collect();
    let mut selected = Vec::new();
    while selected.len() < limit {
        let next = ordered.iter().find_map(|candidate| {
            if introduced.contains(&candidate.card) || allocated.contains(candidate.card.as_str()) {
                return None;
            }
            let waiting_on = match &candidate.prerequisites {
                Prerequisites::NoneRequired => Vec::new(),
                Prerequisites::Routes(routes) => routes
                    .iter()
                    .filter_map(|route| {
                        route
                            .iter()
                            .filter(|parent| !introduced.contains(*parent))
                            .map(|parent| allocated.contains(parent.as_str()).then_some(parent))
                            .collect::<Option<Vec<_>>>()
                    })
                    .min_by(|left, right| left.len().cmp(&right.len()).then(left.cmp(right)))?
                    .into_iter()
                    .cloned()
                    .collect(),
                Prerequisites::Unknown => return None,
            };
            Some((candidate.card.as_str(), waiting_on))
        });
        let Some((card, waiting_on)) = next else {
            break;
        };
        allocated.insert(card);
        selected.push(Selection {
            card: card.to_owned(),
            waiting_on,
        });
    }
    selected
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
