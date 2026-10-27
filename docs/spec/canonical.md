# Spec: canonical branches

Implemented in `crates/og-core/src/canonical.rs`.

There is no global lock on `main`. Each node computes it from the delegates' signed namespaces:

> The canonical tip of a branch is the newest commit that at least `threshold` delegates have in their history of that branch. If there is not exactly one newest such commit, the answer is "no quorum".

Only delegates listed in the current identity document vote, and only through namespaces with signed refs. A contributor's branches never move a canonical branch; they are visible under `refs/peers/<node-id>/heads/*`.

## Algorithm

Given each delegate's tip for the branch:

1. Fewer delegates have the branch than `threshold`: no quorum.
2. All tips equal: that tip.
3. Candidates are the distinct tips plus the merge bases (`git merge-base --all --octopus`) of every subset of 2 to `threshold` distinct tips. Every commit held by `threshold` delegates is an ancestor of one of these, so the newest such commit is always a candidate.
4. A candidate's votes are the delegates whose tip equals it or descends from it. Keep candidates with at least `threshold` votes.
5. Drop any candidate that is an ancestor of another kept candidate. One left: canonical. None: no common history. Several: diverged.

More than 12 distinct tips is reported as diverged rather than searched.

A property test compares this against a brute-force scorer over every commit in random graphs, and checks that the result does not depend on which delegate holds which tip.

## Tags

Tags are not fast-forwarded, so a tag is canonical only when at least `threshold` delegates point it at exactly the same object.

## What Git sees

`git fetch` and `git clone` of `p2p://og:<id>` see:

- canonical branches and tags as `refs/heads/*` and `refs/tags/*`;
- `HEAD` pointing at the default branch if it has a canonical tip;
- every signed namespace's branches and tags under `refs/peers/<node-id>/{heads,tags}/*`. Fetch them explicitly, e.g. `git fetch og refs/peers/<id>/heads/feature:refs/remotes/bob/feature`.

`og canonical` prints each branch's votes and the reason when there is no quorum.
