//! The ogGIT data model: node and repo identities, signed ref manifests, the
//! canonical-branch quorum rule and invite tickets.
//!
//! Nothing in this crate touches the disk or the network, so every rule that
//! decides what a node trusts can be tested in isolation.

pub mod canonical;
pub mod identity;
pub mod json;
pub mod keys;
pub mod refs;
pub mod sigrefs;
pub mod ticket;

pub use canonical::{History, Quorum};
pub use identity::{Doc, RepoId, Revision, Visibility};
pub use keys::{NodeId, SecretKey, Signature};
pub use refs::{Oid, PushUpdate};
pub use sigrefs::SignedRefs;
pub use ticket::{Invite, Role};

/// Ref (inside a namespace) holding the signed ref manifest.
pub const SIGREFS_REF: &str = "refs/og/sigrefs";
/// Ref (inside a namespace) holding the repo identity document history.
pub const IDENTITY_REF: &str = "refs/og/id";

/// Prefix of every peer namespace in a store.
pub fn namespace_prefix(node: &NodeId) -> String {
    format!("refs/namespaces/{node}/")
}
