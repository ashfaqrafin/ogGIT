# Spec: node and repo identity

Implemented in `crates/og-core/src/{keys,identity,json}.rs`.

## Node identity

A node ID is an Ed25519 public key, written as 64 lowercase hex characters. It is also the node's Iroh endpoint ID, so the key that signs data is the key the network dials. The secret key lives in `$OG_HOME/node.key` (hex, mode 0600) and is created on first run.

## Canonical JSON

Signed documents use a subset of RFC 8785: no whitespace, object keys sorted by UTF-16 code units, integers only (at most 2^53 - 1), no floats. Parsers re-encode what they read and reject the input unless the bytes match exactly, so unknown fields and alternative encodings are refused.

## Repo identity document

```json
{"default_branch":"main","delegates":["<node-id>"],"name":"demo","threshold":1,"version":1,"visibility":"public"}
```

| Field | Rule |
| --- | --- |
| `version` | `1` |
| `name` | 1-100 characters of `[A-Za-z0-9._-]` |
| `description` | optional, omitted when empty |
| `default_branch` | a valid branch name |
| `delegates` | 1-255 unique node IDs |
| `threshold` | a strict majority: `threshold * 2 > len(delegates)` and `threshold <= len(delegates)` |
| `visibility` | `public` or `private` |
| `allow` | optional node IDs that may fetch a private repo besides the delegates |
| `previous` | BLAKE3 hex of the previous revision's canonical bytes; absent on revision 0 |

The strict majority means two disjoint groups of delegates can never both reach the threshold, so there is at most one canonical history.

**Repo ID**: `og:` followed by the lowercase RFC 4648 base32 (no padding) of `BLAKE3(canonical bytes of revision 0)`.

## Signatures and storage

A delegate signs `"og/identity/v1\n" || canonical(doc)`. The prefix keeps identity signatures from ever being valid as ref-manifest or invite signatures.

Each revision is a Git commit whose tree holds two blobs:

- `doc.json`: the canonical document.
- `signatures.json`: canonical JSON object mapping node ID to base64 signature.

The history lives at `refs/namespaces/<node-id>/refs/og/id` (first-parent chain, oldest first), and that ref is listed in the namespace's signed refs.

## Verification

1. Revision 0 must hash to the repo ID, have no `previous`, pass validation, and carry a valid signature from at least one of its own delegates.
2. Revision N must pass validation, have `previous` equal to the hash of revision N-1, and carry valid signatures from at least `threshold` of revision N-1's delegates. Signatures from new delegates do not count toward their own admission.
3. A node looks at the identity history in every signed namespace and uses the longest one that verifies (ties broken by the lowest document hash). Histories longer than 1000 revisions are refused.

Not built yet: CLI commands to propose and co-sign revisions (`og circle promote`), key rotation records.
