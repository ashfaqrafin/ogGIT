//! Signed ref manifests ("sigrefs"). Every peer writes only to its own
//! namespace, and after each change signs a manifest listing every ref in it:
//!
//! ```text
//! version 1
//! repo og:<base32>
//! seq 42
//! parent <blake3 hex of the previous manifest, or "none">
//! <oid> refs/heads/main
//! sig <base64 Ed25519 signature over the lines above>
//! ```
//!
//! The `repo` line binds the manifest to one repo so it cannot be replayed into
//! the same node's namespace elsewhere. `seq` strictly increases, which blocks
//! rollback and replay.

use std::collections::BTreeMap;

use crate::{
    SIGREFS_REF,
    identity::RepoId,
    keys::{NodeId, SecretKey, Signature, decode_signature, encode_signature},
    refs::{Oid, validate_ref_name},
};

pub const MAX_MANIFEST_BYTES: usize = 4 << 20;
pub const MAX_REFS: usize = 20_000;

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum SigrefsError {
    #[error("manifest is larger than {MAX_MANIFEST_BYTES} bytes")]
    TooLarge,
    #[error("manifest lists more than {MAX_REFS} refs")]
    TooManyRefs,
    #[error("malformed manifest at line {0}")]
    Malformed(usize),
    #[error("manifest is for {got}, expected {want}")]
    WrongRepo { want: RepoId, got: RepoId },
    #[error("signature does not match namespace owner {0}")]
    BadSignature(NodeId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedRefs {
    pub repo: RepoId,
    pub seq: u64,
    pub parent: Option<String>,
    pub refs: BTreeMap<String, Oid>,
    signature: Signature,
    raw: Vec<u8>,
    body_len: usize,
}

impl SignedRefs {
    /// Builds and signs the manifest that follows `previous`.
    pub fn sign(
        repo: RepoId,
        previous: Option<&SignedRefs>,
        refs: BTreeMap<String, Oid>,
        secret: &SecretKey,
    ) -> Self {
        let seq = previous.map_or(1, |p| p.seq + 1);
        let parent = previous.map(|p| p.hash());
        let mut body = format!(
            "version 1\nrepo {repo}\nseq {seq}\nparent {}\n",
            parent.as_deref().unwrap_or("none")
        );
        for (name, oid) in &refs {
            body.push_str(&format!("{oid} {name}\n"));
        }
        let signature = secret.sign(body.as_bytes());
        let body_len = body.len();
        let raw = format!("{body}sig {}\n", encode_signature(&signature)).into_bytes();
        Self { repo, seq, parent, refs, signature, raw, body_len }
    }

    /// Strictly parses a manifest. Does not check the signature.
    pub fn parse(raw: &[u8]) -> Result<Self, SigrefsError> {
        if raw.len() > MAX_MANIFEST_BYTES {
            return Err(SigrefsError::TooLarge);
        }
        let text = std::str::from_utf8(raw).map_err(|_| SigrefsError::Malformed(0))?;
        let Some(text) = text.strip_suffix('\n') else {
            return Err(SigrefsError::Malformed(0));
        };
        let lines: Vec<&str> = text.split('\n').collect();
        if lines.len() < 5 {
            return Err(SigrefsError::Malformed(lines.len()));
        }

        let field = |i: usize, key: &str| {
            lines[i].strip_prefix(key).and_then(|v| v.strip_prefix(' ')).ok_or(SigrefsError::Malformed(i + 1))
        };
        if field(0, "version")? != "1" {
            return Err(SigrefsError::Malformed(1));
        }
        let repo: RepoId = field(1, "repo")?.parse().map_err(|_| SigrefsError::Malformed(2))?;
        let seq_str = field(2, "seq")?;
        let seq: u64 = seq_str.parse().map_err(|_| SigrefsError::Malformed(3))?;
        if seq == 0 || seq.to_string() != seq_str {
            return Err(SigrefsError::Malformed(3));
        }
        let parent = match field(3, "parent")? {
            "none" => None,
            hex if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) => {
                Some(hex.to_string())
            }
            _ => return Err(SigrefsError::Malformed(4)),
        };

        let last = lines.len() - 1;
        let sig = field(last, "sig")?;
        let signature = decode_signature(sig).ok_or(SigrefsError::Malformed(last + 1))?;

        let mut refs = BTreeMap::new();
        let mut previous_name: Option<&str> = None;
        for (i, line) in lines.iter().enumerate().take(last).skip(4) {
            let malformed = SigrefsError::Malformed(i + 1);
            let (oid, name) = line.split_once(' ').ok_or(malformed.clone())?;
            let oid = Oid::parse(oid).map_err(|_| malformed.clone())?;
            validate_ref_name(name).map_err(|_| malformed.clone())?;
            // Strictly ascending order makes the encoding unique.
            if name == SIGREFS_REF || previous_name.is_some_and(|p| p >= name) {
                return Err(malformed);
            }
            previous_name = Some(name);
            refs.insert(name.to_string(), oid);
            if refs.len() > MAX_REFS {
                return Err(SigrefsError::TooManyRefs);
            }
        }

        let body_len = raw.len() - (lines[last].len() + 1);
        Ok(Self { repo, seq, parent, refs, signature, raw: raw.to_vec(), body_len })
    }

    /// Checks that `owner` signed this manifest for `repo`.
    pub fn verify(&self, owner: &NodeId, repo: &RepoId) -> Result<(), SigrefsError> {
        if self.repo != *repo {
            return Err(SigrefsError::WrongRepo { want: *repo, got: self.repo });
        }
        if !owner.verify(&self.raw[..self.body_len], &self.signature) {
            return Err(SigrefsError::BadSignature(*owner));
        }
        Ok(())
    }

    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn hash(&self) -> String {
        blake3::hash(&self.raw).to_hex().to_string()
    }
}

/// What to do with an incoming manifest given the one already stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// Newer than what we have: accept it.
    Newer,
    /// Identical to what we have.
    Same,
    /// Older than what we have: a rollback or replay.
    Rollback { have: u64, got: u64 },
    /// Same sequence number, different content: the owner signed two
    /// histories. Keep the first one seen.
    Equivocation { seq: u64 },
}

pub fn check_update(current: Option<&SignedRefs>, incoming: &SignedRefs) -> Update {
    let Some(current) = current else { return Update::Newer };
    match incoming.seq.cmp(&current.seq) {
        std::cmp::Ordering::Greater => Update::Newer,
        std::cmp::Ordering::Less => Update::Rollback { have: current.seq, got: incoming.seq },
        std::cmp::Ordering::Equal if incoming.raw == current.raw => Update::Same,
        std::cmp::Ordering::Equal => Update::Equivocation { seq: incoming.seq },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn repo() -> RepoId {
        RepoId::from_bytes([7u8; 32])
    }

    fn oid(c: char) -> Oid {
        Oid::parse(&c.to_string().repeat(40)).unwrap()
    }

    fn sample(sk: &SecretKey, prev: Option<&SignedRefs>) -> SignedRefs {
        let refs = BTreeMap::from([
            ("refs/heads/main".to_string(), oid('a')),
            ("refs/heads/fix".to_string(), oid('b')),
            ("refs/og/id".to_string(), oid('c')),
        ]);
        SignedRefs::sign(repo(), prev, refs, sk)
    }

    #[test]
    fn sign_parse_verify() {
        let sk = SecretKey::generate();
        let m = sample(&sk, None);
        let text = String::from_utf8(m.raw().to_vec()).unwrap();
        assert!(text.starts_with("version 1\nrepo og:"));
        assert!(text.contains("\nseq 1\nparent none\n"));
        let parsed = SignedRefs::parse(m.raw()).unwrap();
        assert_eq!(parsed, m);
        parsed.verify(&NodeId::of(&sk), &repo()).unwrap();
    }

    #[test]
    fn wrong_key_or_repo_fails() {
        let sk = SecretKey::generate();
        let m = sample(&sk, None);
        let other = NodeId::of(&SecretKey::generate());
        assert_eq!(m.verify(&other, &repo()), Err(SigrefsError::BadSignature(other)));
        let elsewhere = RepoId::from_bytes([9u8; 32]);
        assert!(matches!(m.verify(&NodeId::of(&sk), &elsewhere), Err(SigrefsError::WrongRepo { .. })));
    }

    #[test]
    fn tampering_breaks_the_signature() {
        let sk = SecretKey::generate();
        let m = sample(&sk, None);
        let tampered = String::from_utf8(m.raw().to_vec()).unwrap().replace(&"a".repeat(40), &"d".repeat(40));
        let parsed = SignedRefs::parse(tampered.as_bytes()).unwrap();
        assert!(parsed.verify(&NodeId::of(&sk), &repo()).is_err());
    }

    #[test]
    fn rejects_unsorted_and_malformed() {
        let sk = SecretKey::generate();
        let m = sample(&sk, None);
        let text = String::from_utf8(m.raw().to_vec()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let mut swapped = lines.clone();
        swapped.swap(4, 5);
        assert!(SignedRefs::parse((swapped.join("\n") + "\n").as_bytes()).is_err());
        assert!(SignedRefs::parse(text.trim_end().as_bytes()).is_err());
        assert!(SignedRefs::parse(text.replace("seq 1", "seq 01").as_bytes()).is_err());
        assert!(SignedRefs::parse(text.replace("seq 1", "seq 0").as_bytes()).is_err());
        assert!(SignedRefs::parse(b"").is_err());
    }

    #[test]
    fn sequence_rules() {
        let sk = SecretKey::generate();
        let first = sample(&sk, None);
        let second = sample(&sk, Some(&first));
        assert_eq!(second.seq, 2);
        assert_eq!(second.parent, Some(first.hash()));
        assert_eq!(check_update(None, &first), Update::Newer);
        assert_eq!(check_update(Some(&first), &second), Update::Newer);
        assert_eq!(check_update(Some(&second), &second), Update::Same);
        assert_eq!(check_update(Some(&second), &first), Update::Rollback { have: 2, got: 1 });
        let fork = SignedRefs::sign(repo(), Some(&first), BTreeMap::new(), &sk);
        assert_eq!(check_update(Some(&second), &fork), Update::Equivocation { seq: 2 });
    }

    proptest! {
        #[test]
        fn parse_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = SignedRefs::parse(&bytes);
        }

        #[test]
        fn lower_seq_is_always_rejected(a in 1u64..1000, b in 1u64..1000) {
            let sk = SecretKey::generate();
            let mut prev_a = sample(&sk, None);
            prev_a.seq = a - 1;
            let ma = SignedRefs::sign(repo(), Some(&prev_a), BTreeMap::new(), &sk);
            let mut prev_b = sample(&sk, None);
            prev_b.seq = b - 1;
            let mb = SignedRefs::sign(repo(), Some(&prev_b), BTreeMap::new(), &sk);
            let decision = check_update(Some(&ma), &mb);
            prop_assert_eq!(decision == Update::Newer, b > a);
        }

        #[test]
        fn roundtrip(names in proptest::collection::btree_set("[a-z]{1,8}(/[a-z0-9_-]{1,8}){0,2}", 0..20)) {
            let sk = SecretKey::generate();
            let refs = names.into_iter().map(|n| (format!("refs/heads/{n}"), oid('e'))).collect();
            let m = SignedRefs::sign(repo(), None, refs, &sk);
            let parsed = SignedRefs::parse(m.raw()).unwrap();
            prop_assert!(parsed.verify(&NodeId::of(&sk), &repo()).is_ok());
            prop_assert_eq!(parsed, m);
        }
    }
}
