# Building ogGIT: peer-to-peer Git with signed refs

2026-10-22 · Ashfaq Khan Rafin

ogGIT works end to end: a Rust daemon and a Git remote helper replicate ordinary Git repos between peers over Iroh QUIC, and every ref is signed by the person who owns it. Phases 0 and 1 of the six-phase roadmap are built. In tests, two nodes clone, push, merge and converge in about 3 seconds.

This document covers what was built, how the code works (crate by crate, then each command traced through the code), how it was built, what testing found, and what is left.

**Contents**

1. [What was built](#1-what-was-built)
2. [At a glance](#2-at-a-glance)
3. [How a push reaches everyone](#3-how-a-push-reaches-everyone)
4. [The trust model](#4-the-trust-model)
5. [How Git's own protocol rides over QUIC](#5-how-gits-own-protocol-rides-over-quic)
6. [How the code works](#6-how-the-code-works)
7. [Tracing each command through the code](#7-tracing-each-command-through-the-code)
8. [How it was built](#8-how-it-was-built)
9. [Bugs found while testing](#9-bugs-found-while-testing)
10. [Verification](#10-verification)
11. [Security](#11-security)
12. [Not built yet](#12-not-built-yet)
13. [Try it](#13-try-it)

---

## 1. What was built

Seven crates, about 4,500 lines of Rust including tests, produce two binaries: `og` (the CLI, which also runs the daemon) and `git-remote-p2p` (which Git runs for `p2p://` URLs). The rules that decide what to trust live in a crate with no network or disk code, so they can be tested on their own.

| Crate | What it does | Lines of Rust | Tests |
| --- | --- | --- | --- |
| [`og-core`](../crates/og-core/src) | Node and repo identity, canonical JSON, signed ref manifests, the canonical-branch quorum rule, invite tickets. No I/O. | 1,576 | 35 |
| [`og-store`](../crates/og-store/src) | One bare Git repo per repo ID with a namespace per peer. Quarantined fetches, verification, pushes, canonical ref listing, all through the real `git` binary. | 990 | 3 |
| [`og-net`](../crates/og-net/src) | Iroh endpoint setup and the wire messages for `og/fetch/1` and `og/announce/1`. | 167 | 2 |
| [`og-ctl`](../crates/og-ctl/src) | The local control protocol (JSON lines over a Unix socket) and a client that starts the daemon on demand. | 249 | 0 |
| [`og-node`](../crates/og-node/src) | The daemon: keys, saved state, the Iroh router, the fetch bridge, announcement flooding, the control socket. | 820 | covered end to end |
| [`git-remote-p2p`](../crates/git-remote-p2p/src) | Git's remote-helper protocol: `capabilities`, `list`, `fetch`, `push`, `option`. | 230 | 1 |
| [`og-cli`](../crates/og-cli/src) | `og init`, `join`, `follow`, `circle invite`, `sync`, `canonical`, `status`, `daemon`, `stop`, and the hidden `__pipe` bridge. | 440 | 2 |

Around the code: an end-to-end test ([`tests/two_nodes.sh`](../tests/two_nodes.sh)), four specs ([identity](spec/identity.md), [signed refs](spec/sigrefs.md), [canonical branches](spec/canonical.md), [wire protocols](spec/wire.md)), and a status section in the README. It builds against Iroh 1.4.1 and needs Git 2.42 or later.

---

## 2. At a glance

Every machine runs the same stack. A push stays on the pusher's machine; peers hear about it and pull.

```text
       Alice's machine (delegate)                                 Bob's machine (contributor)
 ┌────────────────────────────────────┐                      ┌────────────────────────────────────┐
 │  git ──runs──▶ git-remote-p2p      │                      │  git ──runs──▶ git-remote-p2p      │
 │                  │                 │                      │                  │                 │
 │        og/ctl/1 (Unix socket)      │                      │        og/ctl/1 (Unix socket)      │
 │                  ▼                 │                      │                  ▼                 │
 │              og daemon             │    og/announce/1     │              og daemon             │
 │  signs her refs, announces seq 3   │ ───────────────────▶ │  pulls into a quarantine repo      │
 │                  │                 │ ◀════════════════════│  verifies, then stores             │
 │                  ▼                 │  og/fetch/1: packs   │                  ▼                 │
 │  store: storage/<id>.git           │                      │  store: verified copy of           │
 │  refs/namespaces/<alice>/...       │                      │  Alice's namespace, plus Bob's     │
 └────────────────────────────────────┘                      └────────────────────────────────────┘
```

Crate dependencies point one way, toward the pure core:

```text
og-cli ─────────┬──▶ og-node ──┬──▶ og-net ───▶ og-core
                │              ├──▶ og-store ─▶ og-core
                │              └──▶ og-ctl ───▶ og-core
                └──▶ og-ctl
git-remote-p2p ─────▶ og-ctl, og-core
```

---

## 3. How a push reaches everyone

A push never sends data to anyone. It changes only your own copy and signs it; peers learn about it and pull. Every signature in the system therefore comes from the person who owns the refs it covers.

1. You run `git push og main`. Git starts `git-remote-p2p`, which asks your daemon over its Unix socket to apply the update.
2. The daemon copies the new objects from your working repo into its bare store, checks the update (fast-forward unless forced, branches point to commits), and moves `refs/namespaces/<your-id>/refs/heads/main`.
3. It signs a new manifest of every ref in your namespace with the next sequence number, say `seq 3`, and stores it at `refs/og/sigrefs` in that namespace.
4. It sends `RefsUpdated{repo, node: you, seq: 3}` on `og/announce/1` to every peer it knows for that repo.
5. A peer whose copy is older opens `og/fetch/1` back to you and runs Git's own fetch over the stream into a quarantine repo.
6. The peer checks your signature, that `seq` went up, and that every listed object arrived with its full history. Only then do its copies of your refs move.
7. The peer re-announces to its own peers, so the update reaches nodes that never connected to you. Each `(repo, node, seq)` is handled once, which ends the flood.

In the two-node test, the whole scenario (five pushes, a clone, two pulls and a merge) finishes in about 3 seconds on one machine.

---

## 4. The trust model

There are no accounts and no server that says what `main` is. Trust comes from four pieces of signed data, each checked before it is used. The full formats are in [docs/spec/](spec/).

### Node identity

Each developer has one Ed25519 key, created on first run in `~/.og/node.key`. The same key is the node's Iroh endpoint ID, so the key that signs your refs is the key peers dial. A node ID is 64 hex characters.

### Repo identity document

A repo is born from a small document in canonical JSON (sorted keys, no whitespace, integers only), so it hashes to the same bytes on every peer:

```json
{"default_branch":"main","delegates":["<node-id>"],"name":"demo","threshold":1,"version":1,"visibility":"public"}
```

- The repo ID is `og:` plus the base32 BLAKE3 hash of revision 0, for example `og:ccuuvnnhx7vgsfo2blnwjbealhmo22xesayw7bgggo72zqa6lsqa`.
- Revisions are a chain of Git commits at `refs/og/id`. Revision N is valid only if it links to revision N-1 by hash and carries signatures from `threshold` of the previous delegates.
- `threshold` must be a strict majority of the delegates, so two disjoint groups can never both claim a quorum.

### Signed refs

After every change a node signs a manifest of its whole namespace:

```text
version 1
repo og:ccuuvnnhx7vgsfo2blnwjbealhmo22xesayw7bgggo72zqa6lsqa
seq 3
parent <BLAKE3 hash of the previous manifest>
<commit id> refs/heads/main
<commit id> refs/og/id
sig <Ed25519 signature over the lines above>
```

A receiver applies it only if the owner's signature checks out, `seq` is higher than the one it holds, and every listed object is present. An older `seq` is ignored; the same `seq` with different content is rejected as equivocation.

### Canonical branches

The canonical tip of a branch is the newest commit that at least `threshold` delegates have in their history. If there is not exactly one such commit, `og canonical` reports "no quorum" with the reason, and nothing moves. With one delegate the rule is simply that delegate's tip.

What Git sees on `git clone p2p://og:...`:

- canonical branches and tags as ordinary `refs/heads/*` and `refs/tags/*`, plus `HEAD`;
- every contributor's signed branches under `refs/peers/<node-id>/heads/*`, which a maintainer fetches to review and merge.

---

## 5. How Git's own protocol rides over QUIC

ogGIT invents no transfer format: Git's `upload-pack` protocol, with its have/want negotiation and packfiles, runs unchanged inside an Iroh QUIC stream. The work is in connecting the two ends and checking everything before it lands.

### The bridge, end to end

```text
 fetching node                                                           serving node
 git fetch ──▶ git remote-ext ──▶ og __pipe ──▶ Unix socket ──▶ daemon ═══ QUIC ═══ daemon ──▶ git upload-pack
 (quarantine)   (Git's ext::)     (stdio↔socket)  (private)     (splice)  og/fetch/1         (stdin/stdout)
```

1. The fetching daemon dials the peer by node ID on `og/fetch/1` and sends one JSON line: `{"repo":"og:...","namespace":null,"from":<its address>}`.
2. The serving daemon checks it hosts the repo and that a private repo lists this node, replies `{"ok":true}`, and starts `git upload-pack` on its store.
3. The fetching daemon opens a private Unix socket and runs `git fetch ext::og __pipe <socket>` into a fresh quarantine repo. Git's `ext::` transport starts `og __pipe`, which joins Git's stdin and stdout to that socket.
4. The daemon splices the socket onto the QUIC stream. From here Git talks to Git; the daemons only move bytes.

### Quarantine and checks

The quarantine is a temporary bare repo that borrows the store's objects through `objects/info/alternates`. Git therefore negotiates as if it had the full store and transfers only what is missing, and `fetch.fsckObjects` validates every object. Then, for each peer namespace that arrived:

1. Parse the signed manifest strictly and check the owner's signature and the `repo` line.
2. Compare `seq` with the stored manifest: newer continues, older is ignored as stale, same-but-different is rejected.
3. Run `git rev-list --objects <listed ids> --not --all` with the quarantine visible, so a missing object anywhere in the history fails the namespace.
4. Move the objects into the store (pack data before its index, since Git finds packs through the index), then set the namespace's refs to exactly what the manifest lists, in one `git update-ref --stdin` transaction.

Refs a peer advertises but did not sign are never copied.

---

## 6. How the code works

This section walks through each crate: its main types and functions, what they do, and why they are shaped that way. Links go to the exact lines.

### 6.1 On disk

Everything a node owns lives in one home directory (`$OG_HOME`, default `~/.og`):

```text
~/.og/
├── node.key               Ed25519 secret key, hex, mode 0600
├── state.json             followed repos, their peers, last known peer addresses
├── daemon.lock            held for as long as the daemon runs
├── daemon.log             output of a daemon started on demand
├── run/og.sock            control socket ($XDG_RUNTIME_DIR/og-<hash>/ if this path is too long)
└── storage/
    ├── <base32 repo id>.git      one bare store per repo
    └── tmp/quarantine-*          short-lived quarantine repos, one per fetch
```

Inside a store, every peer has its own namespace, and only its owner ever signs it:

```text
refs/namespaces/<alice>/refs/heads/main
refs/namespaces/<alice>/refs/og/id         → chain of identity revision commits
refs/namespaces/<alice>/refs/og/sigrefs    → blob: Alice's signed manifest
refs/namespaces/<bob>/refs/heads/feature
refs/namespaces/<bob>/refs/og/sigrefs      → blob: Bob's signed manifest
```

There is no separate database of sequence numbers: the last manifest stored for each namespace *is* the record of the last `seq` seen.

### 6.2 `og-core`: the rules, with no I/O

Everything that decides what a node trusts is a pure function here, so it is tested without disks, processes or sockets.

**Keys** ([keys.rs](../crates/og-core/src/keys.rs)). [`NodeId`](../crates/og-core/src/keys.rs#L12) wraps Iroh's `PublicKey`. It adds the traits the rest of the code needs (ordering for `BTreeMap` keys, hashing, serde as a hex string) and a `verify` that returns a plain `bool`. Signatures travel as base64; secret keys are stored as hex ([`secret_from_hex`](../crates/og-core/src/keys.rs#L105)). Because the type wraps Iroh's own key, the node ID printed by `og id` is the same string Iroh uses to dial.

**Canonical JSON** ([json.rs](../crates/og-core/src/json.rs)). Signed documents must produce identical bytes on every machine. [`to_canonical`](../crates/og-core/src/json.rs#L23) converts any serde value to `serde_json::Value`, then [`write`](../crates/og-core/src/json.rs#L41) emits it by hand: object keys sorted by UTF-16 code units (RFC 8785's order), no whitespace, integers only up to 2^53 - 1, floats refused. It sorts keys itself instead of trusting `serde_json`'s map, whose order changes if any dependency enables the `preserve_order` feature. [`from_canonical`](../crates/og-core/src/json.rs#L33) parses, re-encodes and compares byte for byte, so unknown fields and alternative encodings of a signed document are refused.

**Refs** ([refs.rs](../crates/og-core/src/refs.rs)). `Oid` only accepts 40 or 64 lowercase hex characters. [`validate_ref_name`](../crates/og-core/src/refs.rs#L63) is a strict subset of `git check-ref-format`: names come from peers and are later passed to `git`, so anything unusual (`..`, spaces, control characters, `.lock`, `@{`) is refused. `PushUpdate` is the one type shared between the helper, the control protocol and the store: `src` (new object or `None` to delete), `dst`, `force`.

**Repo identity** ([identity.rs](../crates/og-core/src/identity.rs)).

- [`Doc`](../crates/og-core/src/identity.rs#L49) is the identity document. [`Doc::validate`](../crates/og-core/src/identity.rs#L81) enforces the rules: version 1, a safe name, a valid default branch, 1 to 255 unique delegates, and `threshold * 2 > delegates`.
- [`Revision`](../crates/og-core/src/identity.rs#L138) is a document plus a map of delegate signatures. Each signature covers `"og/identity/v1\n" || canonical(doc)`; the prefix stops an identity signature from ever being valid as a manifest or invite signature. [`Revision::repo_id`](../crates/og-core/src/identity.rs#L154) hashes the canonical document with BLAKE3. [`from_parts`](../crates/og-core/src/identity.rs#L160) rebuilds a revision from the two files stored in Git, through `from_canonical`.
- [`verify_history`](../crates/og-core/src/identity.rs#L195) walks revisions oldest first. Revision 0 must hash to the repo ID, have no `previous`, and be signed by at least one of its own delegates. Each later revision must point at the previous one's hash and carry `threshold` valid signatures from the *previous* delegates ([`valid_signers`](../crates/og-core/src/identity.rs#L184)), so a newcomer cannot vote themselves in. It returns the latest valid document.
- [`RepoId`](../crates/og-core/src/identity.rs#L226) is 32 bytes, shown as `og:` plus lowercase base32. Parsing accepts it with or without the prefix.

**Signed refs** ([sigrefs.rs](../crates/og-core/src/sigrefs.rs)). [`SignedRefs`](../crates/og-core/src/sigrefs.rs#L44) keeps the parsed fields *and* the exact bytes it came from (`raw`, and `body_len`, the length before the `sig` line). Keeping the bytes matters: the signature is checked against what was received, never against a re-serialization.

- [`SignedRefs::sign`](../crates/og-core/src/sigrefs.rs#L56) builds the next manifest from the previous one: `seq + 1`, `parent` = BLAKE3 of the previous bytes, the refs sorted by name (a `BTreeMap`), then the signature line.
- [`parse`](../crates/og-core/src/sigrefs.rs#L78) is deliberately strict: a 4 MiB cap, exact line order, no leading zeros in `seq`, ref lines in strictly ascending order (so there is exactly one encoding per ref set), valid object IDs and ref names, and never a listing of `refs/og/sigrefs` itself. A property test feeds it random bytes to confirm it never panics.
- [`verify`](../crates/og-core/src/sigrefs.rs#L138) checks the `repo` line and the owner's signature over `raw[..body_len]`.
- [`check_update`](../crates/og-core/src/sigrefs.rs#L171) compares an incoming manifest with the stored one and returns `Newer`, `Same`, `Rollback` (older) or `Equivocation` (same `seq`, different bytes).

**Canonical branches** ([canonical.rs](../crates/og-core/src/canonical.rs)). The quorum rule needs only two questions about the commit graph, so it asks them through the [`History`](../crates/og-core/src/canonical.rs#L14) trait: `is_ancestor` and `merge_bases`. The store answers with `git merge-base`; tests answer with an in-memory graph. [`branch_quorum`](../crates/og-core/src/canonical.rs#L47) works like this:

1. Fewer delegates have the branch than `threshold`: `TooFewDelegates`.
2. All tips equal: `Agreed` on that tip.
3. Candidates = the distinct tips, plus the merge bases of every subset of 2 to `threshold` distinct tips (iterated as bitmasks, capped at 12 distinct tips). Any commit held by `threshold` delegates is an ancestor of one of these, so the newest such commit is always among them.
4. Count votes: a delegate votes for a candidate if its tip equals it or descends from it. Keep candidates with at least `threshold` votes.
5. Drop candidates that are ancestors of another kept one. One left is `Agreed`, none is `NoCommonHistory`, several is `Diverged`.

[`exact_quorum`](../crates/og-core/src/canonical.rs#L121) handles tags, which never fast-forward: `threshold` delegates must name exactly the same object. Results are the [`Quorum`](../crates/og-core/src/canonical.rs#L25) enum, which `og canonical` prints as an explanation.

**Invites** ([ticket.rs](../crates/og-core/src/ticket.rs)). An [`Invite`](../crates/og-core/src/ticket.rs#L42) holds the repo ID and name, the inviter, a role, an expiry and a list of peer addresses (Iroh `EndpointAddr`s: node ID plus IP and relay addresses). [`encode`](../crates/og-core/src/ticket.rs#L69) signs `"og/invite/v1\n" || canonical(invite)` and returns `og://join/` + base64url of `{invite, sig}`. [`decode`](../crates/og-core/src/ticket.rs#L78) takes the current time as an argument (the crate never reads a clock) and checks size, signature, expiry and that at least one peer is listed.

### 6.3 `og-store`: the trust rules applied to real Git repos

**Running Git** ([git.rs](../crates/og-store/src/git.rs)). Every Git command goes through `Git`, which pins `--git-dir` to the store. [`clean_env`](../crates/og-store/src/git.rs#L35) removes variables such as `GIT_DIR`, `GIT_NAMESPACE` and `GIT_CONFIG_PARAMETERS`. Without that, a daemon started by `git push` would inherit `GIT_DIR` and run every command against the user's repo instead of its store. [`output`](../crates/og-store/src/git.rs#L73) writes stdin from a separate thread so a large input can never deadlock against a full stdout pipe. [`test`](../crates/og-store/src/git.rs#L130) maps exit code 0/1 to true/false for predicate commands such as `merge-base --is-ancestor`.

**The store** ([lib.rs](../crates/og-store/src/lib.rs)). A [`Store`](../crates/og-store/src/lib.rs#L36) is a repo ID, a `Git` handle and a temp directory.

- *Creation.* [`open_or_create`](../crates/og-store/src/lib.rs#L127) runs `git init --bare` and sets config: no auto-gc, `transfer.fsckObjects`, `uploadpack.allowFilter`. [`Store::init`](../crates/og-store/src/lib.rs#L159) creates a brand-new repo: validate the document, sign revision 0, derive the repo ID, create the store, write the identity, sign the refs.
- *Reading refs.* [`namespaces`](../crates/og-store/src/lib.rs#L208) runs one `for-each-ref refs/namespaces/` and groups the result by node ID. Most other operations start from this map.
- *Writing refs.* [`update_refs`](../crates/og-store/src/lib.rs#L238) validates every name, then sends all changes to one `git update-ref --stdin`, which applies them as a single transaction.
- *Signing.* [`sign_refs`](../crates/og-store/src/lib.rs#L268) reads our namespace, compares it with the last manifest, and only if something changed signs the next one, stores it as a blob and moves `refs/og/sigrefs` (with the old value as a compare-and-swap guard).
- *Identity.* [`write_identity`](../crates/og-store/src/lib.rs#L291) stores `doc.json` and `signatures.json` as blobs, builds a tree with `mktree`, and commits it with `commit-tree --no-gpg-sign` on top of the previous revision. [`identity_history`](../crates/og-store/src/lib.rs#L316) walks that chain with `rev-list --first-parent` (at most 1,000 revisions). [`identity`](../crates/og-store/src/lib.rs#L335) checks the history in every signed namespace with `verify_history` and keeps the longest valid one.
- *Canonical view.* [`canonical`](../crates/og-store/src/lib.rs#L362) takes the delegates from the identity, collects each delegate's branch and tag tips from signed namespaces, and runs `branch_quorum` with the store itself as the `History` ([impl](../crates/og-store/src/lib.rs#L622)). [`advertised`](../crates/og-store/src/lib.rs#L402) turns that into what `git fetch` sees: agreed branches and tags, `HEAD` if the default branch has a tip, and every namespace's branches under `refs/peers/<node-id>/`.
- *Pushing.* [`push`](../crates/og-store/src/lib.rs#L441) first copies the objects with a local `git fetch <working repo> <object ids>` over protocol v2, which serves objects no ref points at. [`check_push`](../crates/og-store/src/lib.rs#L480) then decides each ref on its own: only `refs/heads/*` and `refs/tags/*`; branches must point to commits and fast-forward unless forced; existing tags need force. Accepted updates go into one transaction, then `sign_refs` runs.
- *Fetching.* [`fetch_quarantined`](../crates/og-store/src/lib.rs#L517) creates `storage/tmp/quarantine-*`, points its `objects/info/alternates` at the store, and runs `git fetch <url> +refs/namespaces/*:refs/namespaces/*` with protocol version 0, `protocol.ext.allow=always` and `fetch.fsckObjects=true`. [`apply`](../crates/og-store/src/lib.rs#L550) runs [`verify_namespace`](../crates/og-store/src/lib.rs#L590) on every namespace that arrived. Each gets a [`Verdict`](../crates/og-store/src/lib.rs#L92): `Apply`, `Same` or `Stale`, or an error for rejection. For each `Apply`, the listed refs are set, unlisted ones deleted, and `sigrefs` moved. Then [`migrate_objects`](../crates/og-store/src/lib.rs#L643) moves loose objects and packs into the store (each `.idx` last), and one `update_refs` transaction moves every ref. When the `Quarantine` value is dropped, its temp directory is deleted.

### 6.4 `og-net`: wire messages and the endpoint

[`bind`](../crates/og-net/src/lib.rs#L49) builds the Iroh endpoint. In `n0` mode it uses Iroh's `N0` preset (public relays, DNS address lookup); in `local` mode it uses `Minimal` with relays disabled (direct addresses only). Either way it sets the node's secret key, the two ALPNs (`og/fetch/1`, `og/announce/1`), an optional fixed port, and a `MemoryLookup`, an address book the daemon fills from tickets and announcements.

The messages are plain serde types: [`FetchRequest`](../crates/og-net/src/lib.rs#L67) (repo, optional namespace, the requester's address), [`Announcement::RefsUpdated`](../crates/og-net/src/lib.rs#L79), and `Response` with error codes such as `NOT_FOUND` and `FORBIDDEN`. [`read_frame`](../crates/og-net/src/lib.rs#L122) reads one JSON line *byte by byte*, capped at 64 KiB, so it never consumes the first bytes of the Git protocol that follows the header on the same stream.

### 6.5 `og-ctl`: talking to the daemon

The CLI and the helper are short-lived; the daemon is long-lived. They talk over a Unix socket with one JSON object per line: a [`Request`](../crates/og-ctl/src/lib.rs#L51) (`status`, `init`, `list_refs`, `sync`, `push`, `invite`, `join`, `follow`, `canonical`, `shutdown`) answered by a `Reply {ok, error, data}`. Each request has a typed reply struct (`RefList`, `PushReply`, `SyncReply`, ...).

[`run_dir`](../crates/og-ctl/src/lib.rs#L30) handles a Unix quirk: socket paths are limited to about 100 bytes, so a deep `OG_HOME` falls back to `$XDG_RUNTIME_DIR/og-<hash>/`. [`Client::connect_or_spawn`](../crates/og-ctl/src/lib.rs#L180) tries the socket and, if nobody answers, starts `og daemon` in the background. The daemon gets its own process group (so Ctrl-C on a `git` command does not kill it), output appended to `daemon.log`, and every `GIT_*` variable removed. The client polls for up to 20 seconds until the socket answers. [`og_binary`](../crates/og-ctl/src/lib.rs#L236) finds `og` from `$OG_BIN`, then next to the running executable, then on `PATH`. [`call`](../crates/og-ctl/src/lib.rs#L218) sends one request and decodes the typed reply.

### 6.6 `og-node`: the daemon

**Startup** ([`run`](../crates/og-node/src/lib.rs#L46)), in order:

1. Create the home and run directories with mode 0700.
2. Take an exclusive lock on `daemon.lock`. A second daemon for the same home exits here.
3. Bind the control socket (removing a stale one left by a crash).
4. Load or create `node.key`, load `state.json`, and preload every saved peer address into the `MemoryLookup`.
5. Bind the Iroh endpoint and build the shared [`Node`](../crates/og-node/src/lib.rs#L126) value.
6. Start the Iroh [`Router`](../crates/og-node/src/lib.rs#L89), which hands `og/fetch/1` connections to `FetchProtocol` and `og/announce/1` to `AnnounceProtocol`.
7. Start the background sync loop (2 seconds after start, then every `OG_SYNC_INTERVAL` seconds, default 300).
8. Serve the control socket until a `shutdown` request, Ctrl-C or SIGTERM; then remove the socket and close the router and endpoint.

**The `Node` value** ([lib.rs](../crates/og-node/src/lib.rs)) is shared by every task through an `Arc`. Its helpers:

- `my_addr` returns this node's address as peers should dial it. It adds loopback addresses for the bound sockets, so two nodes on one machine always reach each other.
- `learn_peer` records a peer's address in the `MemoryLookup` and `state.json`, and marks it as holding a repo.
- `mark_seen` keeps the set of `(repo, node, seq)` announcements already handled, which is what stops floods.
- `repo_lock` is a per-repo async mutex. Every change to a store (a push, applying a fetch) holds it, so two writers never race on the same refs.
- `blocking` runs store work, which spawns `git` and waits, on Tokio's blocking thread pool so it never stalls network tasks.

**Saved state** ([state.rs](../crates/og-node/src/state.rs#L19)) is two maps: followed repos with their peer sets, and the last known address of each peer. It is written to a temp file and renamed, so a crash never leaves it half-written.

**Peer protocols** ([peer.rs](../crates/og-node/src/peer.rs)):

- [`serve_fetch`](../crates/og-node/src/peer.rs#L69) accepts the stream, reads the `FetchRequest` (10 s timeout), and calls [`check_fetch`](../crates/og-node/src/peer.rs#L125): do we follow this repo, and may this node read it? It then records the requester as a peer, replies `ok`, and spawns `git upload-pack --strict --timeout=120 <store>`. One task copies QUIC → `upload-pack` stdin; the main path copies `upload-pack` stdout → QUIC and finishes the stream. It then waits for the process, and for the client to close the connection, so no tail bytes are cut off.
- [`fetch_from`](../crates/og-node/src/peer.rs#L198) is the client side. It connects, sends the request with its own address, and reads the reply. Then it binds `fetch-<pid>-<n>.sock`, spawns the [bridge task](../crates/og-node/src/peer.rs#L220) (accept one connection, then `copy_bidirectional` between the socket and the QUIC stream), and runs `fetch_quarantined` with an `ext::` URL built by [`ext_url`](../crates/og-node/src/peer.rs#L61). When Git finishes it gives the bridge 5 seconds to drain, removes the socket, closes the connection, takes the repo lock and calls `apply`.
- [`sync`](../crates/og-node/src/peer.rs#L246) runs `fetch_from` against every known peer of a repo at once, collects updated namespaces and errors, and re-announces anything new.
- [`announce`](../crates/og-node/src/peer.rs#L280) sends `RefsUpdated` to every peer of the repo except the one it came from and the namespace owner, each with a 20 s timeout.
- [`serve_announce`](../crates/og-node/src/peer.rs#L144) acknowledges the message at once, then hands it to [`on_refs_updated`](../crates/og-node/src/peer.rs#L159). That ignores repos we don't follow and announcements already seen, compares the announced `seq` with the stored one, fetches from the sender if ours is older, and re-announces on success.

**Control requests** ([ctl.rs](../crates/og-node/src/ctl.rs)): [`handle`](../crates/og-node/src/ctl.rs#L47) dispatches each `Request`. Section 7 traces the important ones.

### 6.7 `git-remote-p2p`: what Git talks to

Git runs `git-remote-p2p <remote> <url>` with `GIT_DIR` set to the user's repo, and speaks a line protocol on stdin/stdout ([main.rs](../crates/git-remote-p2p/src/main.rs)). [`run`](../crates/git-remote-p2p/src/main.rs#L40) parses the URL into a `RepoId`, connects to (or starts) the daemon, and loops over commands:

| Git sends | The helper |
| --- | --- |
| `capabilities` | replies `fetch`, `push`, `option` and a blank line |
| `option verbosity N` / `option progress …` | `ok`; anything else `unsupported` |
| `list` | [`list(false)`](../crates/git-remote-p2p/src/main.rs#L103): asks the daemon to `sync`, then `list_refs`; prints `<oid> <ref>` lines and `@refs/heads/main HEAD` |
| `list for-push` | `list(true)`: our own namespace only, so Git compares against what we last pushed |
| `fetch <oid> <ref>` … blank line | [`fetch`](../crates/git-remote-p2p/src/main.rs#L128): `git fetch --stdin <store>` with the wanted object IDs, into the user's repo; Git then moves the refs itself |
| `push [+]<src>:<dst>` … blank line | [`push`](../crates/git-remote-p2p/src/main.rs#L157): resolves each source with `git rev-parse`, sends one `push` request, prints `ok <dst>` or `error <dst> <reason>` per ref |
| blank line | exits |

`fetch` and `push` arrive in batches ended by a blank line, so the loop reads the rest of the batch before acting. A blank line at the top level ends the session.

### 6.8 `og-cli`: the `og` command

[`main`](../crates/og-cli/src/main.rs#L123) parses arguments with `clap` and, for every command except `daemon`, restores the default SIGPIPE handling so `og status | head` exits quietly. [`run`](../crates/og-cli/src/main.rs#L145) maps each subcommand to one control request and prints the reply. `og daemon` builds a Tokio runtime and calls `og_node::run`. `og stop` sends `shutdown`, then [`wait_for_exit`](../crates/og-cli/src/main.rs#L278) waits until it can take `daemon.lock`, which proves the daemon has exited. [`current_repo`](../crates/og-cli/src/main.rs#L376) finds the repo ID from the current directory's `p2p://` remote, preferring `og`, then `origin`.

The hidden `og __pipe <socket>` ([`pipe`](../crates/og-cli/src/main.rs#L330)) is the far end of the fetch bridge. It connects to the socket and runs two [`pump`](../crates/og-cli/src/main.rs#L349) loops, stdin → socket and socket → stdout, with plain read/write calls (see bug 1 in section 9 for why not `io::copy`).

---

## 7. Tracing each command through the code

### `og init`

1. **CLI** ([run](../crates/og-cli/src/main.rs#L145)): finds the repo top level with `git rev-parse --show-toplevel`, takes the name from the directory and the default branch from `HEAD`, and sends `Request::Init`.
2. **Daemon** ([handle](../crates/og-node/src/ctl.rs#L47)): delegates = this node plus any `--delegate`; threshold defaults to a majority; builds the `Doc`.
3. **Store** ([`Store::init`](../crates/og-store/src/lib.rs#L159)): validates the doc, signs revision 0, derives the repo ID, `git init --bare`, [`write_identity`](../crates/og-store/src/lib.rs#L291), then [`sign_refs`](../crates/og-store/src/lib.rs#L268) (seq 1, listing `refs/og/id`).
4. **Daemon**: adds the repo to `state.json`. **CLI**: `git remote add og p2p://og:<id>` and prints next steps.

### `git push og main`

1. **Git** runs the helper, which sends `list for-push`. The daemon answers with [`own_refs`](../crates/og-store/src/lib.rs#L427).
2. Git decides what to push and sends `push refs/heads/main:refs/heads/main`. The helper resolves the source to an object ID and sends `Request::Push` with its `GIT_DIR`.
3. **Daemon** ([Push arm](../crates/og-node/src/ctl.rs#L88)): takes the repo lock and calls [`Store::push`](../crates/og-store/src/lib.rs#L441): copy objects, `check_push`, one ref transaction, `sign_refs` (seq + 1).
4. It returns per-ref results right away, marks `(repo, me, seq)` seen, and spawns [`announce`](../crates/og-node/src/peer.rs#L280) in the background.
5. **Helper** prints `ok refs/heads/main`; Git updates `refs/remotes/og/main`.

### `og circle invite` and `og join`

1. **Inviter**: `Request::Invite` → the daemon reads the repo name from the identity, waits up to 5 s for a home relay in `n0` mode, and builds an `Invite` with its own address first and up to three other peers. [`Invite::encode`](../crates/og-core/src/ticket.rs#L69) signs it.
2. **Joiner**: `og join <ticket>` sends `Request::Join`. The daemon runs [`Invite::decode`](../crates/og-core/src/ticket.rs#L78) (signature, expiry), `learn_peer` for every address in it, and follows the repo with those peers ([Join arm](../crates/og-node/src/ctl.rs#L121)).
3. [`join_repo`](../crates/og-node/src/ctl.rs#L169) creates an empty store, runs [`sync`](../crates/og-node/src/peer.rs#L246) (a `fetch_from` for each peer), and checks that a valid identity arrived.
4. Inside that fetch, the inviter's [`serve_fetch`](../crates/og-node/src/peer.rs#L69) records the joiner as a peer of the repo, so later pushes are announced to them.
5. **CLI** runs `git clone p2p://og:<id> <dir>`. That goes through the helper's `list` and `fetch`, below.

### `git fetch`, `git pull`, `git clone`

1. **Helper** `list`: `Request::Sync`, so the daemon pulls from every peer first, then `Request::ListRefs`.
2. **Daemon**: [`advertised`](../crates/og-store/src/lib.rs#L402) → [`canonical`](../crates/og-store/src/lib.rs#L362) → [`identity`](../crates/og-store/src/lib.rs#L335) and [`branch_quorum`](../crates/og-core/src/canonical.rs#L47) per branch. It also returns the store path.
3. **Helper** prints the refs. Git asks for the objects it lacks with `fetch <oid> <ref>` lines.
4. **Helper** runs `git fetch --stdin <store>` inside the user's repo, a local protocol v2 fetch by object ID. Git then updates `refs/remotes/...` and, for `pull`, merges.

### An announcement arriving

1. Bob's [`serve_announce`](../crates/og-node/src/peer.rs#L144) reads `RefsUpdated{repo, node: alice, seq: 3, from}` and replies `ok`.
2. [`on_refs_updated`](../crates/og-node/src/peer.rs#L159): repo followed? Not seen before? Stored seq for Alice lower than 3? If so, [`fetch_from`](../crates/og-node/src/peer.rs#L198) the sender.
3. [`apply`](../crates/og-store/src/lib.rs#L550) verifies and stores Alice's namespace at seq 3.
4. Bob re-announces to his other peers, skipping the sender and Alice.

---

## 8. How it was built

The work went bottom-up, and each layer was tested before the next was written on top of it. The scope came from the design itself: build in gated phases, and pass phases 0 and 1 (two nodes and one signed push, then two developers collaborating) before anything else.

### Order of work

1. **Read the design.** The README's roadmap was an embedded block that did not survive as text, so the phase list and exit criteria came from the earlier 28-page implementation guide (PDF) in the repo's history.
2. **Check the ground truth.** Iroh 1.4.1 is newer than the assistant's training data, so its API (endpoint builder, presets, `MemoryLookup`, `Router` and `ProtocolHandler`) was read from the crate's source rather than recalled. Two Git behaviours the design relies on were tested first: `merge-base --octopus --all` exists, and a protocol v2 local fetch serves an object no ref points at. The second matters because the design's suggested trick, `-c uploadpack.allowAnySHA1InWant`, is stripped from a local `upload-pack`.
3. **`og-core`, pure logic.** Identity, manifests, quorum and tickets, with property tests; the quorum rule is checked against a brute-force scorer on random commit graphs.
4. **`og-store` against real `git`.** Tests for push, sync, rollback and forgery between stores reached by local path, before any networking existed.
5. **Networking and processes.** `og-net`, `og-ctl`, the `og-node` daemon, the remote helper and the CLI.
6. **End to end.** A shell test drives two real daemons with plain `git`. The bugs in section 9 were all found here.

### Design decisions

| Decision | Why | Trade-off |
| --- | --- | --- |
| Use the `git` binary for packs, not `gix` | `gix` cannot push; Git's negotiation, `index-pack` and `fsck` are proven | A process per operation; move hot paths to `gix` once benchmarks show where time goes |
| Node ID = Iroh endpoint key | One identity for signing and dialing; Iroh authenticates both ends, so no custom handshake | Rotating a key changes the network address too |
| Bridge with Git's `ext::` transport | Stock `git fetch` drives the whole negotiation over any byte stream | One extra small process per fetch |
| Quarantine repo with alternates | Nothing unverified reaches the store, yet fetches stay incremental | A temporary directory per fetch |
| Flood announcements directly to known peers | Small and fully testable; enough for a circle of collaborators | Messages grow with peer count; `iroh-gossip` is the planned replacement |
| Write canonical JSON by hand | Key order must not change if any dependency turns on `serde_json`'s `preserve_order` | Integers only, a strict subset of RFC 8785 |
| Threshold must be a strict majority | Two disjoint groups can never both reach quorum | A two-delegate repo needs both to agree |
| Keep sequence numbers in Git, not a database | The stored manifest is the record of the last `seq` seen, so it survives restarts with nothing extra | Peer addresses still need a small `state.json` |
| JSON lines for the control socket | The helper stays small and synchronous; easy to debug by hand | No schema versioning yet |

---

## 9. Bugs found while testing

The first end-to-end run hung, and four smaller problems surfaced after it. Unit tests could not catch any of them; they appeared only with real processes talking to each other.

| # | Symptom | Cause | Fix |
| --- | --- | --- | --- |
| 1 | Every fetch hung until `upload-pack` timed out after 120 s | Rust's `io::copy` switches to Linux `splice()`. A splice blocked on Git's stdin stalled the other direction of the same socket, so Git never saw the server's ref list | `og __pipe` uses plain read/write loops ([`pump`](../crates/og-cli/src/main.rs#L349)); the first bytes now arrive at once instead of after stdin closes |
| 2 | `git fetch` warned "rejected: rollback from seq 2 to 1" for the user's own refs | A peer that had not caught up yet served an older manifest, which is normal while an update spreads | Older manifests are ignored quietly as `Stale`; equivocation is still an error |
| 3 | A user with `commit.gpgSign` set would get a GPG prompt during `og init` | `git commit-tree` honours that setting when writing identity revisions | Pass `--no-gpg-sign`; identity revisions carry their own Ed25519 signatures |
| 4 | `og status \| head` panicked | Rust ignores SIGPIPE, so printing to a closed pipe panics | CLI commands restore the default SIGPIPE; the daemon keeps Rust's behaviour |
| 5 | `og stop` returned while the daemon was still exiting | Shutdown closes relay connections, which takes a moment | `og stop` waits until the daemon releases `daemon.lock` |

How bug 1 was found: the daemons' logs showed the connection and the fetch header succeeding, and `upload-pack` had written its 619-byte ref list. A Python stand-in for the server reproduced the hang with no QUIC involved, which isolated `og __pipe`. Timing it showed the first bytes arriving only once stdin closed (4 s in the test), which pointed at the `splice()` specialization in `io::copy`.

---

## 10. Verification

All 43 automated tests pass, `cargo clippy` reports no warnings, and the end-to-end test passes in both network modes. Real `git` is the judge of correctness: after every scenario, `git fsck --strict` is clean in both clones.

| Layer | How it is tested | What it asserts | Result |
| --- | --- | --- | --- |
| Core rules | 35 unit and property tests (`proptest`) | Canonical JSON rejects other encodings; identity revisions need threshold signatures; manifests round-trip, never panic on random bytes, and a lower `seq` is always refused; the quorum rule matches a brute-force scorer and ignores which delegate holds which tip | 35 pass |
| Store | 3 integration tests against real `git` ([store.rs](../crates/og-store/tests/store.rs)) | Push and sync; non-fast-forward needs force; an old manifest is ignored; a namespace forged under another node's ID is rejected on signature; 2-of-3 delegates move `main` | 3 pass |
| Wire, helper, CLI | 5 unit tests | Headers stop at the newline and cap at 64 KiB; `p2p://` URL parsing; durations and repo names | 5 pass |
| Two nodes, direct addresses | [`tests/two_nodes.sh`](../tests/two_nodes.sh), `OG_NETWORK=local` | Init and push; invite, join and clone; a push arrives by announcement with no fetch; a contributor's branch is fetched and merged; a non-delegate's `main` does not become canonical | Pass, about 3 s |
| Two nodes through Iroh relays | `tests/two_nodes.sh`, `OG_NETWORK=n0` | The same scenario with public relays and DNS discovery on; the node registered home relay `aps1-1.relay.n0.iroh.link` | Pass, about 3 s |
| First run | Manual: fresh `OG_HOME`, no daemon | `og init` and `git push` start the daemon on demand; `og stop` shuts it down | Pass |

Not yet tested: two separate machines on different networks, packet loss, large repos, and benchmarks against hosted Git.

---

## 11. Security

Every byte from a peer is treated as hostile: nothing from the network changes a ref until its signature, sequence number and objects have been checked. Of the nine threats below, five are fully met, two partly, and two (spam and key loss) not yet.

| Threat | Defense as built | Status |
| --- | --- | --- |
| Forged ref updates | Manifests are signed by the namespace owner and verified before any ref moves; refs are set only from the signed list, never from what the peer advertised | Built, tested |
| Rollback or replay | `seq` must increase; older manifests are ignored. The `repo` line stops a manifest from being replayed into another repo | Built, tested |
| Equivocation (two histories at one `seq`) | The first manifest seen is kept and the second is rejected | Built, tested |
| Hostile packs and parsers | Quarantine repo, `fetch.fsckObjects`, a connectivity check, strict parsers; limits of 64 KiB per header, 4 MiB and 20,000 refs per manifest, 1,000 identity revisions | Built; caps on pack size and object count still to add |
| Impersonation and man-in-the-middle | Iroh dials by public key and authenticates both ends | Built (Iroh) |
| Reading a private repo | Served only to delegates and the document's `allow` list; others get `FORBIDDEN` | Built; no encryption at rest on seeds |
| Other local users | The control socket sits in a 0700 directory; the key file is 0600 | Built |
| Spam namespaces | Only followed repos are replicated, but any validly signed namespace in them is kept | Not yet: contributor lists and per-node block lists |
| Stolen or lost key | None yet | Not yet: key rotation and recovery |

---

## 12. Not built yet

The foundation phases are done; phases 2 to 6 of the roadmap remain. A few design choices were also changed or deferred on the way.

**Changed from the design:** names use `og` instead of the placeholder `mygit` (`og:` repo IDs, `refs/og/*`, `og/fetch/1`); signed manifests gained a `repo` line; announcements are a direct flood rather than `iroh-gossip`.

| Phase | Remaining work |
| --- | --- |
| 2. Internet P2P | `iroh-gossip` swarms per repo, a DHT from repo ID to peers, mDNS on the LAN, the `og/seed/1` protocol and one-line seeding, quotas, an `og doctor` |
| 3. Circles | Proposing and co-signing identity revisions (`og circle promote`), enforcing invite roles, contributor lists and block lists, key rotation and recovery |
| 4. Collaboration | Issues, patches and reviews as signed objects under `refs/cobs/`, a terminal UI, mirroring out to hosted Git |
| 5. Speed | Racing several peers on clone, partial clone (`--filter=blob:none`), large files via `iroh-blobs`, `git maintenance` on stores, benchmarks against hosted Git |
| 6. Hardening | Fuzzing the parsers, network simulation with `turmoil`, NAT and packet-loss tests, pack size caps, packaging and signed releases |

One quirk to know about today: after a contributor pushes `main`, Git sets their `refs/remotes/<remote>/main` to their own commit. The next `git fetch` resets it to the canonical tip, because only delegates move canonical branches.

---

## 13. Try it

Install both binaries so Git can find the helper on your `PATH`, then work with plain `git`. The daemon starts on first use and keeps its state in `~/.og` (set `OG_HOME` to use another directory).

```bash
cargo install --path crates/og-cli            # og: CLI and daemon
cargo install --path crates/git-remote-p2p    # lets git handle p2p:// URLs

# In an existing Git repo
og init                  # key, identity document, and an `og` remote
git push og main         # signs your refs; peers pull from you
og circle invite         # prints an og://join/... link (valid 7 days)

# On another machine
og join og://join/...    # follows the repo and clones it
git pull                 # syncs with peers, then fetches canonical main

# Anywhere
og status                # peers and each namespace's signed seq
og canonical             # which commit each branch resolves to, and why
og stop                  # stop the daemon
```

Useful settings: `OG_HOME` (home directory), `OG_NETWORK=local|n0` (direct addresses only, or Iroh relays and DNS lookup; default `n0`), `OG_PORT` (fixed UDP port), `OG_SYNC_INTERVAL` (seconds between background syncs), `OG_LOG` (log filter, e.g. `og_node=debug`).

To run the tests: `cargo test --workspace`, then `cargo build && tests/two_nodes.sh` (add `OG_NETWORK=n0` to go through Iroh's relays).
