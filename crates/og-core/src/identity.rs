//! Repo identity. A repo is born from a canonical JSON document; its ID is the
//! BLAKE3 hash of revision 0. Later revisions link to their predecessor by
//! hash and need signatures from `threshold` of the previous delegates, which
//! is how maintainers change without any server.

use std::{collections::BTreeMap, fmt, str::FromStr};

use data_encoding::BASE32_NOPAD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    json::{self, CanonicalError},
    keys::{NodeId, SecretKey, Signature, decode_signature, encode_signature},
};

/// Domain separator for identity signatures, so they can never be confused
/// with a signature over a ref manifest or an invite.
const SIGNING_PREFIX: &[u8] = b"og/identity/v1\n";
const REPO_ID_PREFIX: &str = "og:";

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("{0}")]
    Canonical(#[from] CanonicalError),
    #[error("invalid identity document: {0}")]
    Invalid(String),
    #[error("identity history is empty")]
    Empty,
    #[error("revision 0 hashes to {got}, not to the repo id {want}")]
    WrongRepo { want: RepoId, got: RepoId },
    #[error("revision 0 is not signed by any of its delegates")]
    UnsignedGenesis,
    #[error("revision {0} does not link to the previous revision")]
    BrokenChain(usize),
    #[error("revision {rev} has {have} valid delegate signatures, needs {need}")]
    NotEnoughSignatures { rev: usize, have: usize, need: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Public,
    Private,
}

/// The repo identity document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Doc {
    pub version: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub default_branch: String,
    pub delegates: Vec<NodeId>,
    pub threshold: usize,
    pub visibility: Visibility,
    /// Nodes besides the delegates that may fetch a private repo.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<NodeId>,
    /// BLAKE3 hex of the previous revision. Absent on revision 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
}

impl Doc {
    pub fn new(name: &str, default_branch: &str, delegates: Vec<NodeId>, threshold: usize) -> Self {
        Self {
            version: 1,
            name: name.to_string(),
            description: String::new(),
            default_branch: default_branch.to_string(),
            delegates,
            threshold,
            visibility: Visibility::Public,
            allow: Vec::new(),
            previous: None,
        }
    }

    pub fn validate(&self) -> Result<(), IdentityError> {
        let invalid = |msg: &str| Err(IdentityError::Invalid(msg.to_string()));
        if self.version != 1 {
            return invalid("unsupported version");
        }
        let name_ok = |c: char| c.is_ascii_alphanumeric() || "-_.".contains(c);
        if self.name.is_empty() || self.name.len() > 100 || !self.name.chars().all(name_ok) {
            return invalid("name must be 1-100 characters of [A-Za-z0-9._-]");
        }
        if crate::refs::validate_ref_name(&format!("refs/heads/{}", self.default_branch)).is_err() {
            return invalid("default_branch is not a valid branch name");
        }
        if self.delegates.is_empty() || self.delegates.len() > 255 {
            return invalid("there must be between 1 and 255 delegates");
        }
        let mut sorted = self.delegates.clone();
        sorted.sort();
        sorted.dedup();
        if sorted.len() != self.delegates.len() {
            return invalid("delegates must be unique");
        }
        // A strict majority means two disjoint groups can never both reach the
        // threshold, so there is never more than one canonical history.
        if self.threshold == 0
            || self.threshold > self.delegates.len()
            || self.threshold * 2 <= self.delegates.len()
        {
            return invalid("threshold must be a strict majority of the delegates");
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        // A Doc has no floats or maps with non-string keys, so this cannot fail.
        json::to_canonical(self).expect("Doc serializes canonically")
    }

    pub fn hash(&self) -> blake3::Hash {
        blake3::hash(&self.canonical_bytes())
    }

    pub fn is_delegate(&self, node: &NodeId) -> bool {
        self.delegates.contains(node)
    }

    /// Whether `node` may fetch this repo.
    pub fn can_read(&self, node: &NodeId) -> bool {
        self.visibility == Visibility::Public || self.is_delegate(node) || self.allow.contains(node)
    }

    fn signing_message(&self) -> Vec<u8> {
        [SIGNING_PREFIX, &self.canonical_bytes()].concat()
    }
}

/// One revision of the identity document plus the delegate signatures on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    pub doc: Doc,
    pub signatures: BTreeMap<NodeId, Signature>,
}

impl Revision {
    pub fn new(doc: Doc) -> Self {
        Self { doc, signatures: BTreeMap::new() }
    }

    pub fn sign(&mut self, secret: &SecretKey) {
        let sig = secret.sign(&self.doc.signing_message());
        self.signatures.insert(NodeId::of(secret), sig);
    }

    /// The repo ID this revision would have if it were revision 0.
    pub fn repo_id(&self) -> RepoId {
        RepoId(*self.doc.hash().as_bytes())
    }

    /// Rebuilds a revision from the two files stored in Git, refusing any
    /// encoding other than the canonical one.
    pub fn from_parts(doc: &[u8], signatures: &[u8]) -> Result<Self, IdentityError> {
        let doc: Doc = json::from_canonical(doc)?;
        let raw: BTreeMap<NodeId, String> = json::from_canonical(signatures)?;
        let signatures = raw
            .into_iter()
            .map(|(node, sig)| {
                decode_signature(&sig)
                    .map(|sig| (node, sig))
                    .ok_or_else(|| IdentityError::Invalid(format!("bad signature from {node}")))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { doc, signatures })
    }

    pub fn signatures_bytes(&self) -> Vec<u8> {
        let raw: BTreeMap<String, String> = self
            .signatures
            .iter()
            .map(|(node, sig)| (node.to_string(), encode_signature(sig)))
            .collect();
        json::to_canonical(&raw).expect("signature map serializes canonically")
    }

    /// Delegates of `delegates_of` whose signature on this revision is valid.
    fn valid_signers(&self, delegates_of: &Doc) -> usize {
        let msg = self.doc.signing_message();
        self.signatures
            .iter()
            .filter(|(node, sig)| delegates_of.is_delegate(node) && node.verify(&msg, sig))
            .count()
    }
}

/// Verifies a full identity history (revision 0 first) against `repo` and
/// returns the latest valid document.
pub fn verify_history<'a>(repo: &RepoId, revisions: &'a [Revision]) -> Result<&'a Doc, IdentityError> {
    let genesis = revisions.first().ok_or(IdentityError::Empty)?;
    let got = genesis.repo_id();
    if got != *repo {
        return Err(IdentityError::WrongRepo { want: *repo, got });
    }
    genesis.doc.validate()?;
    if genesis.doc.previous.is_some() {
        return Err(IdentityError::BrokenChain(0));
    }
    if genesis.valid_signers(&genesis.doc) == 0 {
        return Err(IdentityError::UnsignedGenesis);
    }

    let mut current = &genesis.doc;
    for (i, rev) in revisions.iter().enumerate().skip(1) {
        rev.doc.validate()?;
        if rev.doc.previous.as_deref() != Some(current.hash().to_hex().as_str()) {
            return Err(IdentityError::BrokenChain(i));
        }
        let have = rev.valid_signers(current);
        if have < current.threshold {
            return Err(IdentityError::NotEnoughSignatures { rev: i, have, need: current.threshold });
        }
        current = &rev.doc;
    }
    Ok(current)
}

/// A repo ID: `og:` followed by the lowercase base32 BLAKE3 hash of revision 0.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoId([u8; 32]);

#[derive(Debug, thiserror::Error)]
#[error("invalid repo id {0:?}, expected og:<base32>")]
pub struct InvalidRepoId(String);

impl RepoId {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The base32 part, used for directory names.
    pub fn base32(&self) -> String {
        BASE32_NOPAD.encode(&self.0).to_ascii_lowercase()
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{REPO_ID_PREFIX}{}", self.base32())
    }
}

impl fmt::Debug for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RepoId({self})")
    }
}

impl FromStr for RepoId {
    type Err = InvalidRepoId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let b32 = s.strip_prefix(REPO_ID_PREFIX).unwrap_or(s);
        BASE32_NOPAD
            .decode(b32.to_ascii_uppercase().as_bytes())
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .map(Self)
            .ok_or_else(|| InvalidRepoId(s.to_string()))
    }
}

impl Serialize for RepoId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RepoId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(n: usize) -> Vec<SecretKey> {
        (0..n).map(|_| SecretKey::generate()).collect()
    }

    fn genesis(keys: &[SecretKey], threshold: usize) -> Revision {
        let delegates = keys.iter().map(NodeId::of).collect();
        let mut rev = Revision::new(Doc::new("demo", "main", delegates, threshold));
        rev.sign(&keys[0]);
        rev
    }

    fn next(prev: &Revision, edit: impl FnOnce(&mut Doc), signers: &[&SecretKey]) -> Revision {
        let mut doc = prev.doc.clone();
        edit(&mut doc);
        doc.previous = Some(prev.doc.hash().to_hex().to_string());
        let mut rev = Revision::new(doc);
        for k in signers {
            rev.sign(k);
        }
        rev
    }

    #[test]
    fn repo_id_roundtrips() {
        let rev = genesis(&keys(1), 1);
        let id = rev.repo_id();
        let s = id.to_string();
        assert!(s.starts_with("og:") && s.len() == 3 + 52);
        assert_eq!(s.parse::<RepoId>().unwrap(), id);
        assert_eq!(id.base32().parse::<RepoId>().unwrap(), id);
        assert!("og:nope".parse::<RepoId>().is_err());
    }

    #[test]
    fn genesis_verifies() {
        let ks = keys(3);
        let g = genesis(&ks, 2);
        let id = g.repo_id();
        assert_eq!(verify_history(&id, std::slice::from_ref(&g)).unwrap(), &g.doc);
    }

    #[test]
    fn threshold_must_be_majority() {
        let ks = keys(4);
        let ids: Vec<_> = ks.iter().map(NodeId::of).collect();
        assert!(Doc::new("x", "main", ids.clone(), 2).validate().is_err());
        assert!(Doc::new("x", "main", ids.clone(), 3).validate().is_ok());
        assert!(Doc::new("x", "main", ids, 5).validate().is_err());
    }

    #[test]
    fn genesis_must_be_signed_by_a_delegate() {
        let ks = keys(1);
        let mut g = genesis(&ks, 1);
        g.signatures.clear();
        g.sign(&SecretKey::generate());
        assert!(matches!(verify_history(&g.repo_id(), &[g]), Err(IdentityError::UnsignedGenesis)));
    }

    #[test]
    fn revision_needs_threshold_of_previous_delegates() {
        let ks = keys(3);
        let g = genesis(&ks, 2);
        let id = g.repo_id();
        let newcomer = SecretKey::generate();

        let add = |d: &mut Doc| {
            d.delegates.push(NodeId::of(&newcomer));
            d.threshold = 3;
        };
        let one_sig = next(&g, add, &[&ks[0]]);
        assert!(matches!(
            verify_history(&id, &[g.clone(), one_sig]),
            Err(IdentityError::NotEnoughSignatures { have: 1, need: 2, .. })
        ));

        // The newcomer's own signature does not count: they are not a delegate yet.
        let with_newcomer = next(&g, add, &[&ks[0], &newcomer]);
        assert!(matches!(
            verify_history(&id, &[g.clone(), with_newcomer]),
            Err(IdentityError::NotEnoughSignatures { have: 1, need: 2, .. })
        ));

        let ok = next(&g, add, &[&ks[0], &ks[2]]);
        let history = [g, ok];
        let latest = verify_history(&id, &history).unwrap();
        assert_eq!(latest.delegates.len(), 4);
    }

    #[test]
    fn broken_chain_is_rejected() {
        let ks = keys(1);
        let g = genesis(&ks, 1);
        let mut r1 = next(&g, |d| d.description = "x".into(), &[&ks[0]]);
        r1.doc.previous = Some("00".repeat(32));
        r1.signatures.clear();
        r1.sign(&ks[0]);
        assert!(matches!(verify_history(&g.repo_id(), &[g, r1]), Err(IdentityError::BrokenChain(1))));
    }

    #[test]
    fn parts_roundtrip_and_reject_noncanonical() {
        let g = genesis(&keys(2), 2);
        let doc = g.doc.canonical_bytes();
        let sigs = g.signatures_bytes();
        assert_eq!(Revision::from_parts(&doc, &sigs).unwrap(), g);

        let mut pretty = serde_json::to_vec_pretty(&g.doc).unwrap();
        assert!(Revision::from_parts(&pretty, &sigs).is_err());
        pretty = doc.clone();
        pretty.splice(1..1, br#""extra":1,"#.iter().copied());
        assert!(Revision::from_parts(&pretty, &sigs).is_err());
    }
}
