#!/usr/bin/env bash
# End-to-end test: two nodes on one machine replicate a repo using plain git.
#
#   cargo build && tests/two_nodes.sh
#
# Alice creates a repo and pushes; Bob joins from an invite link and clones;
# Alice's next push reaches Bob through an announcement; Bob's branch reaches
# Alice; Bob cannot move the canonical main because he is not a delegate.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${OG_TEST_BIN:-$ROOT/target/debug}
WORK=${OG_TEST_DIR:-$(mktemp -d)}
rm -rf "$WORK" && mkdir -p "$WORK"
WORK=$(cd "$WORK" && pwd)

export PATH="$BIN:$PATH"
export OG_NETWORK=${OG_NETWORK:-local} OG_SYNC_INTERVAL=3600 OG_LOG=${OG_LOG:-warn,og_node=debug}
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$WORK/gitconfig"
git config --file "$GIT_CONFIG_GLOBAL" user.name "Test"
git config --file "$GIT_CONFIG_GLOBAL" user.email "test@example.com"
git config --file "$GIT_CONFIG_GLOBAL" init.defaultBranch main

ALICE="$WORK/alice-home" BOB="$WORK/bob-home"
as_alice() { OG_HOME="$ALICE" "$@"; }
as_bob() { OG_HOME="$BOB" "$@"; }

step() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() { printf '\033[31mFAIL: %s\033[0m\n' "$*"; exit 1; }
pass() { printf '\033[32mok\033[0m %s\n' "$*"; }

cleanup() {
  as_alice og stop >/dev/null 2>&1 || true
  as_bob og stop >/dev/null 2>&1 || true
  wait 2>/dev/null || true
}
trap cleanup EXIT

# Waits up to 20s for a command to succeed.
eventually() {
  for _ in $(seq 1 100); do "$@" >/dev/null 2>&1 && return 0; sleep 0.2; done
  return 1
}

step "start two daemons"
mkdir -p "$ALICE" "$BOB"
as_alice og daemon >"$WORK/alice.log" 2>&1 &
as_bob og daemon >"$WORK/bob.log" 2>&1 &
eventually as_alice og id || fail "alice's daemon did not start"
eventually as_bob og id || fail "bob's daemon did not start"
ALICE_ID=$(as_alice og id) BOB_ID=$(as_bob og id)
pass "alice ${ALICE_ID:0:10}, bob ${BOB_ID:0:10}"

step "alice creates a repo and pushes"
mkdir "$WORK/alice" && cd "$WORK/alice"
git init -q
echo "hello" > README && git add README && git commit -qm "first commit"
as_alice og init
REPO=$(git remote get-url og | sed 's|p2p://||')
as_alice git push -q og main
FIRST=$(git rev-parse HEAD)
pass "pushed $FIRST to $REPO"

step "bob joins from an invite link"
TICKET=$(as_alice og circle invite 2>/dev/null)
[[ $TICKET == og://join/* ]] || fail "bad ticket: $TICKET"
cd "$WORK"
as_bob og join "$TICKET" bob
cd "$WORK/bob"
[[ $(git rev-parse HEAD) == "$FIRST" ]] || fail "bob's clone is not at alice's commit"
git fsck --no-progress --strict 2>&1 | grep -v '^Checking' && fail "fsck complained" || true
pass "bob cloned $FIRST"

step "alice pushes again; the announcement reaches bob without a fetch"
cd "$WORK/alice"
echo "more" >> README && git commit -qam "second commit"
SECOND=$(git rev-parse HEAD)
as_alice git push -q og main
bob_has_second() { as_bob og canonical --repo "$REPO" | grep -q "${SECOND:0:12}"; }
eventually bob_has_second || fail "bob's node never received alice's second push"
pass "bob's node has $SECOND"
cd "$WORK/bob" && as_bob git pull -q
[[ $(git rev-parse HEAD) == "$SECOND" ]] || fail "git pull did not reach the second commit"
pass "git pull fast-forwarded to $SECOND"

step "bob pushes a feature branch; alice merges it"
git checkout -qb feature
echo "feature" > FEATURE && git add FEATURE && git commit -qm "bob's feature"
FEATURE=$(git rev-parse HEAD)
as_bob git push -q origin feature
cd "$WORK/alice"
alice_has_feature() { as_alice og status | grep -q "${BOB_ID:0:10} seq"; }
eventually alice_has_feature || fail "alice's node never received bob's namespace"
as_alice git fetch -q og "refs/peers/$BOB_ID/heads/feature:refs/remotes/bob/feature"
[[ $(git rev-parse bob/feature) == "$FEATURE" ]] || fail "alice could not fetch bob's branch"
git merge -q --no-edit bob/feature
as_alice git push -q og main
MERGED=$(git rev-parse HEAD)
pass "alice merged bob's feature as $MERGED"

step "bob pulls the merge"
cd "$WORK/bob" && git checkout -q main
as_bob git pull -q
[[ $(git rev-parse HEAD) == "$MERGED" ]] || fail "bob did not get the merge"
[[ -f FEATURE ]] || fail "merged file missing"
pass "bob is at $MERGED"

step "bob is not a delegate, so his main does not become canonical"
echo "rogue" >> README && git commit -qam "bob's unreviewed change"
as_bob git push -q origin main
as_bob git fetch -q origin
[[ $(git rev-parse origin/main) == "$MERGED" ]] || fail "canonical main moved without the delegate"
as_bob og canonical | grep -q "${MERGED:0:12}" || fail "canonical view is wrong"
pass "canonical main stays at $MERGED"

step "both stores are consistent"
for dir in "$WORK/alice" "$WORK/bob"; do
  git -C "$dir" fsck --no-progress --strict >/dev/null 2>&1 || fail "fsck failed in $dir"
done
as_alice og status
pass "all checks passed"
