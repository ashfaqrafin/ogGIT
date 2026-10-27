# Spec: wire and local protocols

Implemented in `crates/og-net`, `crates/og-node/src/peer.rs` and `crates/og-ctl`.

## Peer protocols

Connections are Iroh QUIC connections. Iroh authenticates both ends by node ID, so there is no custom handshake. Each job has its own ALPN with the version in the name.

Every stream begins with one JSON object on one line from the client (at most 64 KiB) and one JSON line in reply:

```json
{"ok":true}
{"ok":false,"error":"NOT_FOUND","message":"not hosting og:..."}
```

Error codes: `NOT_FOUND`, `FORBIDDEN`, `BAD_REQUEST`, `BUSY`, `INTERNAL`.

### `og/fetch/1`

```json
{"repo":"og:...","namespace":null,"from":{"id":"<node-id>","addrs":[...]}}
```

- `namespace`: `null` for every namespace the server holds, or one node ID (served with `GIT_NAMESPACE`).
- `from`: how to reach the client. The server records it as a peer of the repo so it can announce to it later. It is used only when `from.id` matches the authenticated connection.

After an `ok` reply the stream carries Git's `upload-pack` protocol (version 0) in both directions: the server runs `git upload-pack --strict --timeout=120 <store>`. A private repo is served only to its delegates and `allow` list.

On the client, Git drives the fetch itself: the daemon runs `git fetch ext::<og> __pipe <socket> +refs/namespaces/*:refs/namespaces/*` into a quarantine repo, and `og __pipe` connects Git's stdio to a private Unix socket that the daemon splices onto the QUIC stream. Verification then follows [sigrefs.md](sigrefs.md).

### `og/announce/1`

```json
{"type":"RefsUpdated","repo":"og:...","node":"<node-id>","seq":42,"from":{...}}
```

"I hold `node`'s signed refs for `repo` at `seq`." A receiver that follows the repo and has an older `seq` fetches from the sender, then re-announces to its other peers. Each `(repo, node, seq)` is handled once, which stops the flood.

This is a direct flood to known peers. The design names `iroh-gossip` for this; a gossip swarm per repo is the planned replacement once networks are larger than a few dozen peers.

## Discovery

In order: addresses carried in invite tickets, addresses learned from `from` fields, then, in `n0` network mode, Iroh's DNS address lookup and relays. In `local` mode there are no relays and no lookups. Not built yet: mDNS, a DHT from repo ID to peers.

## Invite tickets

`og://join/<base64url, no padding, of canonical JSON>`:

```json
{"invite":{"expires":1793282987,"inviter":"<node-id>","name":"demo","peers":[{"id":"<node-id>","addrs":[...]}],"repo":"og:...","role":"contributor","version":1},"sig":"<base64>"}
```

The signature is by `inviter` over `"og/invite/v1\n" || canonical(invite)`. `peers` lists the inviter first, then up to three other holders as seed hints. `role` is recorded but not enforced yet.

## Local control protocol (`og/ctl/1`)

The `og` CLI and `git-remote-p2p` talk to the daemon over a Unix socket at `$OG_HOME/run/og.sock` (or `$XDG_RUNTIME_DIR/og-<hash>/og.sock` when that path would be too long), in a 0700 directory. Requests and replies are JSON lines:

```json
{"cmd":"list_refs","repo":"og:...","for_push":false}
{"ok":true,"data":{"refs":[["<oid>","refs/heads/main"]],"head":"refs/heads/main","store":"/home/me/.og/storage/....git"}}
```

Commands: `status`, `init`, `list_refs`, `sync`, `push`, `invite`, `join`, `follow`, `canonical`, `shutdown`. Clients start the daemon if the socket is not answering.

## Remote helper

Git runs `git-remote-p2p` for `p2p://og:<id>` URLs. It implements the `fetch`, `push` and `option` capabilities:

| Git sends | Helper does |
| --- | --- |
| `list` | asks the daemon to sync with peers, then prints canonical and peer refs |
| `list for-push` | prints the node's own namespace |
| `fetch <oid> <ref>` batch | `git fetch --stdin <store>` of the wanted object IDs into the user's repo |
| `push <src>:<dst>` batch | resolves each source, asks the daemon to update and sign the namespace, prints `ok`/`error` per ref |
