# Building a P2P Git

Oct 21, 2026 · @Ashfaq Khan

A P2P Git is a Rust daemon plus a git remote helper that replicates ordinary Git repos between peers over QUIC, with signed refs for trust. Build it in six phases, starting with two nodes and one signed push.

## At a glance

A push never leaves your machine unsigned: your daemon writes and signs it, announces it, and peers pull it.

&#91;embedded content: architecture · two machines running the same stack\]

Every machine runs the same stack. The daemon is the trust boundary, because nothing reaches the store until its signatures check out.

## Requirements

Each of your seven goals maps to one concrete design choice. Only the first needs a caveat: "completely decentralized" in practice means no *required* central party, so optional bootstrap, relay and seed nodes are fine as long as anyone can replace them.

| # | Your goal | How it is met | Main component |
| --- | --- | --- | --- |
| 1 | Completely decentralized | Keys instead of accounts. Every peer can hold a full copy of a repo. | Ed25519 identity, signed refs |
| 2 | P2P, Freenet-style | Direct QUIC connections dialed by public key, gossip for updates, a DHT for discovery. Peers host only the repos they choose to follow. | Iroh, gossip, DHT |
| 3 | Fast download and upload | Reuse Git packfiles, fetch from several peers in parallel, run always-on seed nodes. | Pack negotiation, multi-peer fetch |
| 4 | Easy developer circle | A signed identity document lists delegates and a signature threshold. Invite links add contributors. | Repo identity document |
| 5 | Seamless and super fast | Rust. A long-running daemon keeps connections warm, so `git push` pays no connection setup. | Daemon, remote helper |
| 6 | Easy to set up | One static binary. First run creates keys, starts the daemon and finds peers. | CLI |
| 7 | Helpful to developers | Works with plain `git`. Issues and patches travel with the code and work offline. | `git-remote-p2p`, collaborative objects |

Use Rust, not C++. Both are fast enough, but parsing untrusted peer data is where C++ network code collects its security bugs, and the best building blocks (Iroh, gitoxide) are Rust.

## Prior art

Reuse Radicle's trust model and Iroh's transport, then differentiate on speed and setup instead of rebuilding either. Decide what yours does better before writing code, or you will end up with a second Radicle.

| Project | Take this | Watch out for |
| --- | --- | --- |
| [Radicle](https://radicle.dev/guides/protocol) | Node IDs as Ed25519 keys, a repo ID derived from a signed identity document at `refs/rad/id`, per-peer ref namespaces, signed refs at `refs/rad/sigrefs`, gossip announcements, issues and patches under `refs/cobs/` | Repo IDs are SHA-1 digests. Its protocol guide removed the transport-encryption section over a disclosed vulnerability, so do not copy its custom handshake. |
| [Iroh](https://docs.rs/crate/iroh/latest) (1.4.0, 2026-10-20) | QUIC connections dialed by public key, hole punching, encrypted relay fallback. The [1.0 post](https://iroh.computer/blog/the-road-to-iroh-1-0) says the wire format and API are stable. | Address lookup is centralized by default (an optional mainline DHT mode exists). Relays trade some P2P purity for reliability. |
| [gitoxide](https://github.com/GitoxideLabs/gitoxide) (`gix`) | Pure-Rust clone, fetch, packfile and commit-graph code for reading and verifying repos | Its project page lists push as not implemented, so use Git's own plumbing for building and receiving packs. |
| libp2p | An alternative network stack with a DHT and gossip built in | More modular and heavier than Iroh, so more setup work. |

## Stack and workspace

Start with one Cargo workspace of eight small crates, so the pure logic (identity, signatures) can be tested without a network. The suggested layout:

```text
mygit/
├── Cargo.toml            # workspace root
├── crates/
│   ├── core/             # identity, repo document, signed refs (no I/O)
│   ├── store/            # bare repos, namespaces, signature checks
│   ├── net/              # Iroh endpoint and the wire protocols
│   ├── gossip/           # inventory and ref announcements
│   ├── node/             # the daemon (mygitd) and its local socket
│   ├── remote-helper/    # git-remote-p2p binary
│   ├── cobs/             # issues and patches (added in phase 4)
│   └── cli/              # mygit binary
├── docs/spec/            # written specs: identity, sigrefs, wire
└── tests/                # simulation and integration tests
```

| Purpose | Use | Note |
| --- | --- | --- |
| Async runtime | `tokio` | Iroh is built on it |
| Transport | `iroh` 1.x | QUIC, hole punching, relays, dial by key |
| Announcements | `iroh-gossip` | Separate crate from `iroh` |
| Large files | `iroh-blobs` | BLAKE3 content-addressed transfer |
| Read and verify Git data | `gix` | No push support, so read-side only |
| Build and receive packs | system `git` | `pack-objects`, `index-pack`, `upload-pack`, `receive-pack` |
| Signatures and hashing | `ed25519-dalek`, `blake3` | Both already in Iroh's dependency tree |
| Serialization | `serde` with canonical JSON (RFC 8785) | Signed documents must serialize byte-for-byte the same on every peer |
| Node metadata | `redb` | Embedded, pure Rust key-value store |
| CLI and logs | `clap`, `ratatui`, `tracing`, `thiserror` |  |

One decision saves months: in version 1, call the real `git` binary for pack work and write Rust only for identity, trust, networking and the remote helper. Move hot paths to `gix` later, once benchmarks show where time goes.

## Core specs

Write these four specs in `docs/spec/` before the code that depends on them. Everything else (daemon, helper, UI) is plumbing around them.

### 1. Node identity

Each developer has one Ed25519 keypair. Use the same key as the Iroh endpoint key, so the network address and the signing identity are one thing: the node ID is the public key. There are no accounts and no servers to register with.

### 2. Repo identity document

A repo is born from a small canonical JSON document, stored as a Git blob at `refs/mygit/id`. The repo ID is the BLAKE3 hash of revision 0, written as `mg:` plus base32. Use BLAKE3 rather than SHA-1.

```json
{
  "version": 1,
  "name": "myproject",
  "default_branch": "main",
  "delegates": ["<node-id-A>", "<node-id-B>", "<node-id-C>"],
  "threshold": 2,
  "visibility": "public"
}
```

Later revisions are valid only if signed by at least `threshold` of the *current* delegates and linked to the previous revision by hash. This is how you add or remove maintainers without any server.

### 3. Signed refs

Every peer writes only to its own namespace, `refs/namespaces/<node-id>/...`. After each push the node rewrites a manifest at `refs/namespaces/<node-id>/refs/mygit/sigrefs` and signs it:

```text
version 1
seq 42
parent <hash of previous manifest>
<commit-oid> refs/heads/main
<commit-oid> refs/heads/fix-parser
sig <base64 Ed25519 signature over the lines above>
```

A receiver accepts a manifest only if the signature matches the namespace owner, `seq` is higher than the last one it saw (blocks rollback and replay), and every listed commit exists locally after the fetch.

### 4. Canonical default branch

There is no global lock on `main`. Instead, compute it: look at each delegate's own `main`, and the canonical tip is the newest commit that at least `threshold` delegates have in their history. If delegates diverge so that no commit reaches the threshold, report "no quorum" instead of guessing. This is the rule that replaces "GitHub says this is main".

### 5. Wire protocol

Iroh already authenticates both ends by public key, so you do not need a custom handshake. Use one protocol name (ALPN) per job and put the version in the name; Iroh's own team [advises against elaborate versioning schemes](https://iroh.computer/blog/the-road-to-iroh-1-0).

| ALPN | Carries | Details |
| --- | --- | --- |
| `mygit/fetch/1` | Repo data | Client sends `{repo_id, namespace}`, then both sides pipe Git's own `upload-pack` protocol over the stream |
| `mygit/seed/1` | Asking a seed to host a repo | Client sends `{repo_id, node_id, seq}`. The seed then fetches from the client over `mygit/fetch/1` and verifies sigrefs before accepting. |
| `mygit/announce/1` | Inventory and ref updates | `Inventory{repo_ids}` and `RefsUpdated{repo_id, node_id, seq, sigrefs_oid}`, sent over gossip |
| `mygit/ctl/1` | Local IPC | Helper and CLI talk to the daemon over a Unix socket, not the network |

## Git remote helper

`git-remote-p2p` is a small program that Git runs whenever a remote URL starts with `p2p://`. Put it on your `PATH` and plain Git works with your network: `git clone p2p://mg:<repo-id>`, `git fetch`, `git push`.

One rule shapes the whole design: a push never sends data to someone else. It writes to your own local store, signs your new ref manifest with your key, and announces the update. Peers and seeds then pull from you. That is why every signature in the system comes from the person who owns the namespace.

| Git sends | Helper replies | What your daemon does |
| --- | --- | --- |
| `capabilities` | `fetch`, `push`, `option`, blank line | Nothing |
| `list` or `list for-push` | `<oid> <refname>` per line, `@refs/heads/main HEAD`, blank line | Returns the canonical refs (or your namespace for push) from the local store |
| `fetch <oid> <ref>` (batch, ends with a blank line) | Blank line when objects are in place | Syncs from peers into the store, verifying sigrefs |
| `push <src>:<dst>` (batch, ends with a blank line) | `ok <dst>` or `error <dst> <reason>` per ref, blank line | Updates your namespace, signs a new sigrefs manifest, gossips `RefsUpdated`, pings seeds |
| `option <name> <value>` | `unsupported` at first | Nothing |

The helper itself stays tiny. It parses these lines and calls the daemon over a Unix socket, so every `git` command skips network setup:

```rust
use std::io::{self, BufRead, Write};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let _remote = args.next();                    // e.g. "origin"
    let url = args.next().unwrap_or_default();    // "p2p://mg:..."
    let repo_id = url.trim_start_matches("p2p://");
    let daemon = DaemonClient::connect()?;        // Unix socket to mygitd

    let mut out = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let line = line?;
        match line.split_once(' ').unwrap_or((line.as_str(), "")) {
            ("capabilities", _) => writeln!(out, "fetch\npush\noption\n")?,
            ("list", _) => {
                for (oid, name) in daemon.list_refs(repo_id)? {
                    writeln!(out, "{oid} {name}")?;
                }
                writeln!(out)?;
            }
            ("option", _) => writeln!(out, "unsupported")?,
            ("fetch", _) => { /* read the batch, daemon.sync(), local git fetch */ }
            ("push", _) => { /* read the batch, daemon.push(), print ok/error */ }
            ("", _) => break,                     // blank line ends the session
            _ => {}
        }
        out.flush()?;
    }
    Ok(())
}
```

For `fetch`, the simplest way to get objects into the user's repo is to let the daemon sync peers into its own bare store, then run a local `git fetch` from that store with `-c uploadpack.allowAnySHA1InWant=true`, so Git's own code does the copying. Test the helper by hand first: pipe `capabilities` and `list` into it before wiring up the network.

## Replication and speed

Replication is announce, then pull: a peer says "my refs for repo X are at sequence 42", and interested peers fetch the difference using Git's normal negotiation. Speed comes mostly from not inventing anything new on the data path.

### How a push reaches everyone

1. You run `git push`. The helper hands it to your daemon, which updates your namespace and signs a new manifest (`seq` 42).
2. The daemon publishes `RefsUpdated{repo_id, node_id, seq, sigrefs_oid}` on the gossip topic for that repo (the topic is the hash of the repo ID).
3. Each follower compares `seq` with the last one it stored. If it is newer, it opens `mygit/fetch/1` to the announcer and runs Git's `upload-pack` over the stream.
4. The follower verifies the signature, the sequence number and that every listed commit arrived. Only then does it update its copy of your namespace.
5. It re-announces, so the update spreads through peers that never talked to you directly. Duplicate announcements are dropped by `(node_id, seq)`.

### How peers find each other

Try these in order, cheapest first:

1. **Invite ticket.** The link you send a collaborator contains your node ID and seed node IDs, so they connect directly with no lookup.
2. **LAN discovery.** mDNS finds teammates on the same network.
3. **Configured seeds and bootstrap nodes.** A short list in the config file. They only introduce peers, and nothing breaks if one disappears.
4. **DHT lookup.** Maps repo ID to peers hosting it. Iroh's default address lookup is centralized, with an [optional mainline DHT mode](https://iroh.computer/blog/the-road-to-iroh-1-0), so decide early whether you run your own lookup service or rely on that mode.

### Speed techniques

| Technique | What it gives you | Notes |
| --- | --- | --- |
| Reuse Git packfiles and have/want negotiation | Smallest possible transfer, no new object format | The biggest win for the least work |
| Persistent daemon with warm connections | No connection setup per `git` command | Iroh tries a direct path first and falls back to relays when hole punching fails |
| Race two or three peers | Fast clone even if one peer is slow | Start the fetch on all, keep the first to finish, cancel the rest |
| Seed nodes in several regions | Always-on source close to users | The largest real-world gain for popular repos |
| Partial clone (`--filter=blob:none`) | Small first clone of huge repos | Seeds must set `uploadpack.allowFilter` |
| `commit-graph` and `multi-pack-index` | Fast `log`, `merge-base` and fetch negotiation | Have the daemon run `git maintenance` on its stores |
| Large files as content-addressed blobs | Verified streaming from many peers | Keep them out of Git; use `iroh-blobs` and commit small pointer files |

True multi-source downloads, where different peers send different pieces of one pack, need protocol work Git does not provide. Ship "race the peers" first, and split work by ref or namespace only after benchmarks justify it.

Make seeding a one-liner (`mygit seed <repo-id>`, or one Docker command). Availability is the weak point of every P2P Git, and a few always-on seeds fix most of it.

## Developer circle

The circle is not a separate feature. It is the delegate list and threshold in the repo identity document, plus a few commands that make adding people painless.

| Role | Can do | How it is enforced |
| --- | --- | --- |
| Delegate | Count toward the canonical `main`, merge patches, change the identity document | Listed in the signed identity document |
| Contributor | Push to their own namespace, open patches and issues, comment | Their own key; others accept or ignore their namespace |
| Follower | Fetch, read, and re-host the repo | No permission needed |
| Seed | Host the repo around the clock without collaborating | A node running `mygit seed <repo-id>` |

```bash
mygit init                              # make keys and the identity doc; prints mg:<id>
mygit circle invite --role contributor  # prints mygit://join/<ticket>
mygit join mygit://join/<ticket>        # clone and start following
mygit circle promote <node-id>          # propose a delegate; needs threshold signatures
mygit patch open | review | merge
mygit issue new
```

An invite ticket carries the repo ID, the inviter's node ID, a few seed hints, an expiry date and the inviter's signature. Joining needs no account and no approval step: the new person pushes to their own namespace and sends a patch.

### Patches and issues without a server

Store each issue, patch, comment and review as a small signed change in Git itself, under `refs/cobs/org.mygit.issue/<id>` and `refs/cobs/org.mygit.patch/<id>`. This is the approach [Radicle](https://radicle.dev/guides/protocol) uses for its collaborative objects: each change is a Git commit forming a graph, peers merge graphs, and state is rebuilt by replaying changes in topological order with commit hashes breaking ties. Because replay is deterministic, two people editing offline never conflict, and the objects sync with the same replication as code.

The merge flow for a patch:

1. A contributor pushes a branch to their namespace and runs `mygit patch open`. A signed patch object records the base and head commits.
2. Delegates fetch the head, review (each review is another signed change), and test.
3. A delegate merges locally and pushes to their own `main`.
4. When `threshold` delegates have the same commit in their `main` history, the canonical `main` moves. No single person can rewrite it.

To keep spam out, show only objects from authors the delegates have listed as contributors, and let each node keep its own block list.

## Security

Treat every byte from a peer as hostile. The rule: nothing from the network changes a ref or the object store until its hash, signature and sequence number have been checked.

| Threat | Defense |
| --- | --- |
| Forged ref updates | Every manifest is signed by the namespace owner's key and checked before use. |
| Rollback or replay of an old manifest | A strictly increasing `seq`. Store the last one seen per node and reject anything lower. |
| Hostile packs (decompression bombs, giant deltas, corrupt objects) | Receive into a quarantine directory. Run `git index-pack --strict` with `--max-input-size`, enable `fetch.fsckObjects`, cap object counts and delta depth, then move objects into the store. |
| Impersonation and man-in-the-middle | Iroh dials by public key and authenticates both ends, so connect only to the key you expect. |
| Snooping relays | Relays only forward encrypted packets, per Iroh's docs. They can still see who talks to whom and when. |
| Spam and Sybil nodes | Replicate only repos you follow. Add per-peer rate limits, connection caps and block lists. |
| Stolen or lost key | Sign a key-rotation record with the old key, let delegates revise the identity document, and offer an encrypted key backup. Support hardware-key signing early. |
| Weak hash in Git objects | Do not rely on SHA-1 alone: your repo IDs use BLAKE3 and your manifests are signed. Check current Git SHA-256 support before choosing an object format. |

### Private repos

Start with an allow list: the identity document gets a `visibility` field and a list of node IDs, and your node serves the repo only to those keys. [Radicle](https://radicle.dev/guides/protocol) does the same. Do not promise end-to-end encrypted hosting on untrusted seeds. Seeds cannot negotiate or build packs over ciphertext, so that is a research project for later, not a version 1 feature.

## Testing and benchmarks

Speed is one of your seven goals, so measure it from the first working push, not at the end. Use real `git` as the judge of correctness: if `git fsck` and `git log` agree after a clone, the layer works.

| Layer | Tool | What to assert |
| --- | --- | --- |
| Signatures, identity document, canonical branch | `proptest` | Merge rules give the same result for any arrival order; a lower `seq` is always rejected |
| Issue and patch merging | `proptest` | Replaying the same changes in any order gives the same state |
| Parsers (sigrefs, identity document, wire frames) | `cargo-fuzz` | No panics, no unbounded memory, on random input |
| Network behaviour | `turmoil` | Convergence after partitions, delays and dropped messages, deterministically |
| NAT and packet loss | Docker with `tc netem`, plus a real VM behind home-style NAT | Hole punching works, relay fallback works, transfers finish under 5% loss |
| Git compatibility | Shell tests that run `git clone p2p://`, `push`, `fetch` | `git fsck` clean, refs identical, shallow and partial clones work |
| Performance | `criterion`, `hyperfine` | See targets below |

Benchmark against GitHub on three repo sizes (a few MB, a few hundred MB, a multi-GB monorepo) and report medians of at least ten runs. These are starting targets to adjust after you have numbers:

- Push visible on a connected peer within 2 seconds.
- Clone from a warm seed no slower than the same clone from GitHub.
- Idle daemon under 50 MB of memory.
- `git status` and `git log` unaffected by the helper, since they never touch it.

## Roadmap

Build in six phases and do not start one until the previous gate passes, so you never carry a broken foundation forward.

&#91;embedded content: roadmap · 6 phases, a gate after each\]

The durations on phases 0 and 1 are rough estimates. Size the later phases once the MVP shows your real pace.

## Risks and open decisions

The code is the easier half. The risks that sink P2P Git projects are about availability and adoption.

| Risk | Why it happens | What to do |
| --- | --- | --- |
| Repo unreachable | With no servers, if nobody online holds it, nobody can clone | Always-on seeds, one-line seeding, pinning of important repos |
| Unfriendly names | Repo IDs are hashes | Optional signed aliases or a DNS TXT record pointing to the ID, added after version 1 |
| Data cannot be deleted | Once replicated, copies live on other peers | Say so plainly in the docs; never store secrets in a repo |
| Hidden central points | Iroh's default lookup and relays are run by one party | Run your own relay and lookup service, and make the addresses configurable |
| Adoption | Developers will not switch for equal features | Win on one visible thing, such as setup in under a minute or faster clones from seeds |

Decide these before phase 2:

- Iroh or libp2p for the network layer. This guide assumes Iroh.
- Whether your issue and patch format should be compatible with Radicle's, which would let the two networks share tooling.
- Which discovery mechanism you commit to for repo ID to peers.
- Project name and license, so the first public crate has a home.

## Sources

Pages opened on 2026-10-21:

- [Radicle protocol guide](https://radicle.dev/guides/protocol)
- [iroh crate documentation](https://docs.rs/crate/iroh/latest)
- [The road to iroh 1.0](https://iroh.computer/blog/the-road-to-iroh-1-0)
- [gitoxide project page](https://github.com/GitoxideLabs/gitoxide)

Git's remote-helper protocol, `index-pack` options and `uploadpack.*` settings are written from memory of Git's documentation, not fetched. Check them against `git help gitremote-helpers`, `git help index-pack` and `git help config` before relying on them.
