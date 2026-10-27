//! Git object IDs and ref names as they appear in signed data.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum RefError {
    #[error("invalid object id {0:?}")]
    Oid(String),
    #[error("invalid ref name {0:?}")]
    Name(String),
}

/// A Git object ID: 40 (SHA-1) or 64 (SHA-256) lowercase hex characters.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Oid(String);

impl Oid {
    pub fn parse(s: &str) -> Result<Self, RefError> {
        let valid_len = s.len() == 40 || s.len() == 64;
        if valid_len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(Self(s.to_string()))
        } else {
            Err(RefError::Oid(s.to_string()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Oid {
    type Error = RefError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<Oid> for String {
    fn from(oid: Oid) -> Self {
        oid.0
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", &self.0[..12])
    }
}

/// Checks a full ref name (`refs/...`) against a strict subset of Git's
/// `check-ref-format` rules. Names come from peers, so anything unusual is
/// refused rather than passed to `git`.
pub fn validate_ref_name(name: &str) -> Result<(), RefError> {
    let bad = || RefError::Name(name.to_string());
    if !name.starts_with("refs/") || name.len() > 1024 || name.ends_with('/') {
        return Err(bad());
    }
    if name.contains("..") || name.contains("@{") || name.contains("//") {
        return Err(bad());
    }
    let forbidden = |c: char| c.is_ascii_control() || " ~^:?*[\\\x7f".contains(c);
    if name.chars().any(forbidden) {
        return Err(bad());
    }
    for component in name.split('/') {
        if component.is_empty() || component.starts_with('.') || component.ends_with(".lock") {
            return Err(bad());
        }
    }
    if name.ends_with('.') {
        return Err(bad());
    }
    Ok(())
}

/// One ref update requested by `git push`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushUpdate {
    /// New value, or `None` to delete the ref.
    pub src: Option<Oid>,
    /// Destination ref, e.g. `refs/heads/main`.
    pub dst: String,
    pub force: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oids() {
        assert!(Oid::parse(&"a".repeat(40)).is_ok());
        assert!(Oid::parse(&"0".repeat(64)).is_ok());
        assert!(Oid::parse(&"A".repeat(40)).is_err());
        assert!(Oid::parse("abc").is_err());
    }

    #[test]
    fn ref_names() {
        for ok in ["refs/heads/main", "refs/tags/v1.0", "refs/heads/feature/x-y_z"] {
            assert!(validate_ref_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "heads/main",
            "refs/heads/",
            "refs/heads/a..b",
            "refs/heads/a b",
            "refs/heads/.hidden",
            "refs/heads/x.lock",
            "refs/heads/a\nb",
            "refs//x",
            "refs/heads/x@{1}",
        ] {
            assert!(validate_ref_name(bad).is_err(), "{bad}");
        }
    }
}
