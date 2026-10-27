//! Canonical branches by quorum. There is no global lock on `main`; it is
//! computed: the canonical tip is the newest commit that at least `threshold`
//! delegates have in their history. If no single newest commit exists the
//! answer is "no quorum", never a guess.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{keys::NodeId, refs::Oid};

/// Commit-graph queries the quorum rule needs. The store implements this with
/// `git merge-base`; tests use an in-memory graph.
pub trait History {
    type Error;
    /// Whether `ancestor` is reachable from `descendant` (a commit is not its
    /// own strict ancestor, but callers treat equality separately).
    fn is_ancestor(&self, ancestor: &Oid, descendant: &Oid) -> Result<bool, Self::Error>;
    /// The best common ancestors of all of `oids`.
    fn merge_bases(&self, oids: &[Oid]) -> Result<Vec<Oid>, Self::Error>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Quorum {
    Agreed { oid: Oid, votes: usize },
    TooFewDelegates { have: usize, need: usize },
    Diverged { candidates: Vec<Oid> },
    NoCommonHistory,
}

impl Quorum {
    pub fn oid(&self) -> Option<&Oid> {
        match self {
            Quorum::Agreed { oid, .. } => Some(oid),
            _ => None,
        }
    }
}

/// More distinct delegate tips than this and we refuse to search every
/// subset for merge bases. Real repos have a handful of delegates.
const MAX_DISTINCT_TIPS: usize = 12;

/// Applies the quorum rule to one branch. `tips` maps each delegate that has
/// the branch to its tip.
pub fn branch_quorum<H: History>(
    history: &H,
    tips: &BTreeMap<NodeId, Oid>,
    threshold: usize,
) -> Result<Quorum, H::Error> {
    let threshold = threshold.max(1);
    if tips.len() < threshold {
        return Ok(Quorum::TooFewDelegates { have: tips.len(), need: threshold });
    }
    let distinct: Vec<&Oid> = tips.values().collect::<BTreeSet<_>>().into_iter().collect();
    if distinct.len() == 1 {
        return Ok(Quorum::Agreed { oid: distinct[0].clone(), votes: tips.len() });
    }
    if distinct.len() > MAX_DISTINCT_TIPS {
        return Ok(Quorum::Diverged { candidates: distinct.into_iter().cloned().collect() });
    }

    // Any commit held by `threshold` delegates is an ancestor of the merge
    // bases of the distinct tips those delegates hold, so the newest such
    // commit is either a tip or one of those merge bases.
    let mut candidates: BTreeSet<Oid> = distinct.iter().map(|o| (*o).clone()).collect();
    let max_size = threshold.min(distinct.len());
    for mask in 1u32..(1 << distinct.len()) {
        let size = mask.count_ones() as usize;
        if size < 2 || size > max_size {
            continue;
        }
        let subset: Vec<Oid> =
            (0..distinct.len()).filter(|i| mask & (1 << i) != 0).map(|i| distinct[i].clone()).collect();
        candidates.extend(history.merge_bases(&subset)?);
    }

    let contains = |commit: &Oid, tip: &Oid| -> Result<bool, H::Error> {
        if commit == tip { Ok(true) } else { history.is_ancestor(commit, tip) }
    };
    let mut qualified = Vec::new();
    for candidate in candidates {
        let mut votes = 0;
        for tip in tips.values() {
            if contains(&candidate, tip)? {
                votes += 1;
            }
        }
        if votes >= threshold {
            qualified.push((candidate, votes));
        }
    }

    let mut newest = Vec::new();
    for (candidate, votes) in &qualified {
        let mut superseded = false;
        for (other, _) in &qualified {
            if other != candidate && history.is_ancestor(candidate, other)? {
                superseded = true;
                break;
            }
        }
        if !superseded {
            newest.push((candidate.clone(), *votes));
        }
    }

    Ok(match newest.len() {
        0 => Quorum::NoCommonHistory,
        1 => {
            let (oid, votes) = newest.remove(0);
            Quorum::Agreed { oid, votes }
        }
        _ => Quorum::Diverged { candidates: newest.into_iter().map(|(oid, _)| oid).collect() },
    })
}

/// Quorum for refs that are never fast-forwarded, such as tags: `threshold`
/// delegates must point at exactly the same object.
pub fn exact_quorum(tips: &BTreeMap<NodeId, Oid>, threshold: usize) -> Quorum {
    let threshold = threshold.max(1);
    if tips.len() < threshold {
        return Quorum::TooFewDelegates { have: tips.len(), need: threshold };
    }
    let mut counts: BTreeMap<&Oid, usize> = BTreeMap::new();
    for oid in tips.values() {
        *counts.entry(oid).or_default() += 1;
    }
    let agreed: Vec<_> = counts.iter().filter(|(_, n)| **n >= threshold).collect();
    match agreed.as_slice() {
        [(oid, votes)] => Quorum::Agreed { oid: (**oid).clone(), votes: **votes },
        _ => Quorum::Diverged { candidates: counts.keys().map(|o| (*o).clone()).collect() },
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use proptest::prelude::*;

    use super::*;
    use crate::keys::SecretKey;

    /// An in-memory commit graph: commit `i` has the listed parents.
    #[derive(Debug)]
    struct Graph {
        parents: Vec<Vec<usize>>,
    }

    fn oid(i: usize) -> Oid {
        Oid::parse(&format!("{i:040x}")).unwrap()
    }

    fn index(o: &Oid) -> usize {
        usize::from_str_radix(o.as_str(), 16).unwrap()
    }

    impl Graph {
        fn ancestors_or_self(&self, c: usize) -> BTreeSet<usize> {
            let mut seen = BTreeSet::new();
            let mut stack = vec![c];
            while let Some(n) = stack.pop() {
                if seen.insert(n) {
                    stack.extend(&self.parents[n]);
                }
            }
            seen
        }
    }

    impl History for Graph {
        type Error = Infallible;

        fn is_ancestor(&self, a: &Oid, d: &Oid) -> Result<bool, Infallible> {
            Ok(a != d && self.ancestors_or_self(index(d)).contains(&index(a)))
        }

        fn merge_bases(&self, oids: &[Oid]) -> Result<Vec<Oid>, Infallible> {
            let common = oids
                .iter()
                .map(|o| self.ancestors_or_self(index(o)))
                .reduce(|a, b| a.intersection(&b).copied().collect())
                .unwrap_or_default();
            let best = common
                .iter()
                .filter(|c| !common.iter().any(|d| d != *c && self.ancestors_or_self(*d).contains(c)))
                .map(|c| oid(*c))
                .collect();
            Ok(best)
        }
    }

    fn delegates(n: usize) -> Vec<NodeId> {
        let mut ids: Vec<_> = (0..n).map(|_| NodeId::of(&SecretKey::generate())).collect();
        ids.sort();
        ids
    }

    fn tips(ds: &[NodeId], at: &[usize]) -> BTreeMap<NodeId, Oid> {
        ds.iter().zip(at).map(|(d, c)| (*d, oid(*c))).collect()
    }

    // 0 - 1 - 2 - 3
    //      \
    //       4 - 5
    fn sample() -> Graph {
        Graph { parents: vec![vec![], vec![0], vec![1], vec![2], vec![1], vec![4]] }
    }

    #[test]
    fn single_delegate_is_its_own_tip() {
        let d = delegates(1);
        let q = branch_quorum(&sample(), &tips(&d, &[3]), 1).unwrap();
        assert_eq!(q, Quorum::Agreed { oid: oid(3), votes: 1 });
    }

    #[test]
    fn two_of_three_on_one_line() {
        let d = delegates(3);
        // Delegates at 2, 3 and 5: commit 2 is in two histories (2 and 3).
        let q = branch_quorum(&sample(), &tips(&d, &[2, 3, 5]), 2).unwrap();
        assert_eq!(q, Quorum::Agreed { oid: oid(2), votes: 2 });
    }

    #[test]
    fn falls_back_to_merge_base() {
        let d = delegates(3);
        // 3 and 5 diverged at 1; 1 is the newest commit two delegates share.
        let q = branch_quorum(&sample(), &tips(&d, &[3, 5, 0]), 2).unwrap();
        assert_eq!(q, Quorum::Agreed { oid: oid(1), votes: 2 });
    }

    #[test]
    fn too_few_delegates() {
        let d = delegates(1);
        let q = branch_quorum(&sample(), &tips(&d, &[3]), 2).unwrap();
        assert_eq!(q, Quorum::TooFewDelegates { have: 1, need: 2 });
    }

    #[test]
    fn diverged_reports_no_quorum() {
        // 0 - 1 - 2 ; 0 - 3 ; 4 merges 2 and 3.
        let g = Graph { parents: vec![vec![], vec![0], vec![1], vec![0], vec![2, 3]] };
        let d = delegates(3);
        // Commits 2 and 3 each have two votes and neither contains the other.
        let q = branch_quorum(&g, &tips(&d, &[2, 3, 4]), 2).unwrap();
        assert_eq!(q, Quorum::Diverged { candidates: vec![oid(2), oid(3)] });
    }

    #[test]
    fn tags_need_exact_agreement() {
        let d = delegates(3);
        assert_eq!(exact_quorum(&tips(&d, &[1, 1, 2]), 2), Quorum::Agreed { oid: oid(1), votes: 2 });
        assert!(matches!(exact_quorum(&tips(&d, &[1, 2, 3]), 2), Quorum::Diverged { .. }));
    }

    /// Reference answer: score every commit in the graph.
    fn brute_force(g: &Graph, at: &[usize], threshold: usize) -> Option<usize> {
        let histories: Vec<_> = at.iter().map(|t| g.ancestors_or_self(*t)).collect();
        let qualified: Vec<usize> = (0..g.parents.len())
            .filter(|c| histories.iter().filter(|h| h.contains(c)).count() >= threshold)
            .collect();
        let newest: Vec<usize> = qualified
            .iter()
            .copied()
            .filter(|c| !qualified.iter().any(|q| q != c && g.ancestors_or_self(*q).contains(c)))
            .collect();
        (newest.len() == 1).then(|| newest[0])
    }

    fn graph_and_tips() -> impl Strategy<Value = (Graph, Vec<usize>, usize)> {
        (2usize..14).prop_flat_map(|n| {
            let parents = (0..n)
                .map(|i| {
                    if i == 0 {
                        Just(vec![]).boxed()
                    } else {
                        proptest::collection::btree_set(0..i, 1..=2.min(i))
                            .prop_map(|s| s.into_iter().collect())
                            .boxed()
                    }
                })
                .collect::<Vec<_>>();
            (parents, proptest::collection::vec(0..n, 1..6), 0usize..3)
                .prop_map(|(parents, tips, extra)| (Graph { parents }, tips, extra))
        })
    }

    proptest! {
        #[test]
        fn matches_brute_force((g, at, extra) in graph_and_tips()) {
            let threshold = (at.len() / 2 + 1 + extra).min(at.len());
            let d = delegates(at.len());
            let q = branch_quorum(&g, &tips(&d, &at), threshold).unwrap();
            prop_assert_eq!(q.oid().map(index), brute_force(&g, &at, threshold));
            if let Quorum::Agreed { votes, .. } = q {
                prop_assert!(votes >= threshold);
            }
        }

        #[test]
        fn independent_of_which_delegate_holds_which_tip((g, at, _) in graph_and_tips(), seed in any::<u64>()) {
            let threshold = at.len() / 2 + 1;
            let d = delegates(at.len());
            let mut shuffled = at.clone();
            let len = shuffled.len();
            shuffled.rotate_left((seed as usize) % len);
            let a = branch_quorum(&g, &tips(&d, &at), threshold).unwrap();
            let b = branch_quorum(&g, &tips(&d, &shuffled), threshold).unwrap();
            prop_assert_eq!(a, b);
        }
    }
}
