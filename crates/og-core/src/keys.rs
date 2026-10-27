//! Node identity. A node ID is an Ed25519 public key, the same key that
//! identifies the node's Iroh endpoint, so the network address and the signing
//! identity are one thing.

use std::{cmp::Ordering, fmt, hash::Hash, str::FromStr};

use data_encoding::BASE64;
pub use iroh_base::{PublicKey, SecretKey, Signature};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct NodeId(PublicKey);

#[derive(Debug, thiserror::Error)]
#[error("invalid node id: {0}")]
pub struct InvalidNodeId(String);

impl NodeId {
    pub fn of(secret: &SecretKey) -> Self {
        Self(secret.public())
    }

    pub fn public_key(&self) -> PublicKey {
        self.0
    }

    pub fn verify(&self, msg: &[u8], sig: &Signature) -> bool {
        self.0.verify(msg, sig).is_ok()
    }

    /// The first ten hex characters, for display only.
    pub fn short(&self) -> String {
        self.to_string()[..10].to_string()
    }
}

impl From<PublicKey> for NodeId {
    fn from(key: PublicKey) -> Self {
        Self(key)
    }
}

impl Ord for NodeId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.as_bytes().cmp(other.0.as_bytes())
    }
}

impl PartialOrd for NodeId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Hash for NodeId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.as_bytes().hash(state)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Lowercase hex, identical to Iroh's endpoint ID display.
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.short())
    }
}

impl FromStr for NodeId {
    type Err = InvalidNodeId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PublicKey::from_str(s).map(Self).map_err(|_| InvalidNodeId(s.to_string()))
    }
}

impl Serialize for NodeId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

pub fn encode_signature(sig: &Signature) -> String {
    BASE64.encode(&sig.to_bytes())
}

pub fn decode_signature(s: &str) -> Option<Signature> {
    let bytes: [u8; 64] = BASE64.decode(s.as_bytes()).ok()?.try_into().ok()?;
    Some(Signature::from_bytes(&bytes))
}

/// Parses a secret key from the hex form written to the key file.
pub fn secret_from_hex(s: &str) -> Option<SecretKey> {
    let bytes: [u8; 32] = data_encoding::HEXLOWER_PERMISSIVE
        .decode(s.trim().as_bytes())
        .ok()?
        .try_into()
        .ok()?;
    Some(SecretKey::from_bytes(&bytes))
}

pub fn secret_to_hex(secret: &SecretKey) -> String {
    data_encoding::HEXLOWER.encode(&secret.to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_roundtrips_through_string_and_json() {
        let id = NodeId::of(&SecretKey::generate());
        assert_eq!(id.to_string().parse::<NodeId>().unwrap(), id);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<NodeId>(&json).unwrap(), id);
    }

    #[test]
    fn signatures_roundtrip() {
        let sk = SecretKey::generate();
        let sig = sk.sign(b"hello");
        let decoded = decode_signature(&encode_signature(&sig)).unwrap();
        assert!(NodeId::of(&sk).verify(b"hello", &decoded));
        assert!(!NodeId::of(&sk).verify(b"other", &decoded));
    }

    #[test]
    fn secret_hex_roundtrip() {
        let sk = SecretKey::generate();
        let back = secret_from_hex(&secret_to_hex(&sk)).unwrap();
        assert_eq!(back.to_bytes(), sk.to_bytes());
    }
}
