# Spec: signed refs

Implemented in `crates/og-core/src/sigrefs.rs`, enforced in `crates/og-store/src/lib.rs`.

Every node writes only to its own namespace, `refs/namespaces/<node-id>/refs/...`. After each change it signs a manifest of every ref in that namespace and stores it as a blob at `refs/namespaces/<node-id>/refs/og/sigrefs`.

## Format

```text
version 1
repo og:<base32 repo id>
seq 42
parent <BLAKE3 hex of the previous manifest's bytes, or "none">
<oid> refs/heads/main
<oid> refs/og/id
sig <base64 Ed25519 signature over every byte above this line>
```

- Lines end in `\n`, including the last.
- `seq` is a decimal integer of at least 1 with no leading zeros. The first manifest has `seq 1`.
- Ref lines are sorted by name in strictly ascending byte order, so each set of refs has one encoding. Names must pass a strict subset of `git check-ref-format`, and `refs/og/sigrefs` itself is never listed.
- Object IDs are 40 or 64 lowercase hex characters.
- Limits: 4 MiB per manifest, 20,000 refs.

The `repo` line is an addition to the original design: without it, a manifest signed for one repo could be replayed into the same node's namespace in another repo.

## Accepting a manifest from a peer

Objects from a peer are fetched into a quarantine repo (a temporary bare repo that borrows the store's objects through `objects/info/alternates`, with `fetch.fsckObjects` on). For each namespace the peer sent:

1. The manifest must parse, name this repo, and carry a valid signature from the namespace's owner.
2. Compare `seq` with the stored manifest:
   - higher: continue;
   - equal and identical: nothing to do;
   - lower: **stale**, ignored without error (a peer that has not caught up looks the same as a replay, and neither is applied);
   - equal but different: **equivocation**, rejected.
3. Every listed object and everything it references must be present: `git rev-list --objects <oids> --not --all` must succeed with the quarantine visible.
4. The namespace's refs are then set to exactly what the manifest lists. Refs the peer advertised but did not sign are never copied. Objects move from quarantine into the store before any ref changes, and all ref changes happen in one `git update-ref --stdin` transaction.

## Pushing

`git push` hands updates to the daemon, which copies the objects from the working repo (protocol v2 local fetch by object ID), checks each update (branches must point to commits and fast-forward unless forced; existing tags need force; only `refs/heads/*` and `refs/tags/*` are accepted), applies them, and signs a new manifest. Each ref succeeds or fails on its own, as with `git push`.
