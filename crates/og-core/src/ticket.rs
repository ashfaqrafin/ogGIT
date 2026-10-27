//! Invite tickets: `og://join/<base64url>`. A ticket carries the repo ID, how
//! to reach the inviter (and optionally seeds), an expiry date and the
//! inviter's signature, so joining needs no lookup and no account.

use data_encoding::BASE64URL_NOPAD;
pub use iroh_base::EndpointAddr;
use serde::{Deserialize, Serialize};

use crate::{
    identity::RepoId,
    json,
    keys::{NodeId, SecretKey, decode_signature, encode_signature},
};

pub const TICKET_PREFIX: &str = "og://join/";
const SIGNING_PREFIX: &[u8] = b"og/invite/v1\n";
const MAX_TICKET_LEN: usize = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum TicketError {
    #[error("not an og://join/ ticket")]
    Format,
    #[error("ticket is not signed by its inviter")]
    BadSignature,
    #[error("ticket expired")]
    Expired,
    #[error("ticket lists no peers to connect to")]
    NoPeers,
}

/// What the inviter expects the new member to do. Recorded in the ticket;
/// roles are not enforced until circles land.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Contributor,
    Follower,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invite {
    pub version: u32,
    pub repo: RepoId,
    /// Repo name, used as the default clone directory.
    pub name: String,
    pub inviter: NodeId,
    pub role: Role,
    /// Unix seconds.
    pub expires: u64,
    /// The inviter first, then seed hints.
    pub peers: Vec<EndpointAddr>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signed {
    invite: Invite,
    sig: String,
}

impl Invite {
    fn signing_message(&self) -> Vec<u8> {
        let body = json::to_canonical(self).expect("invite serializes canonically");
        [SIGNING_PREFIX, &body].concat()
    }

    /// Signs the ticket with the inviter's key and encodes it.
    pub fn encode(&self, secret: &SecretKey) -> String {
        let sig = encode_signature(&secret.sign(&self.signing_message()));
        let signed = Signed { invite: self.clone(), sig };
        let bytes = json::to_canonical(&signed).expect("ticket serializes canonically");
        format!("{TICKET_PREFIX}{}", BASE64URL_NOPAD.encode(&bytes))
    }

    /// Decodes a ticket and checks its signature and expiry against `now`
    /// (Unix seconds).
    pub fn decode(ticket: &str, now: u64) -> Result<Self, TicketError> {
        let ticket = ticket.trim();
        if ticket.len() > MAX_TICKET_LEN {
            return Err(TicketError::Format);
        }
        let encoded = ticket.strip_prefix(TICKET_PREFIX).unwrap_or(ticket);
        let bytes = BASE64URL_NOPAD.decode(encoded.as_bytes()).map_err(|_| TicketError::Format)?;
        let Signed { invite, sig } = serde_json::from_slice(&bytes).map_err(|_| TicketError::Format)?;
        if invite.version != 1 {
            return Err(TicketError::Format);
        }
        let sig = decode_signature(&sig).ok_or(TicketError::BadSignature)?;
        if !invite.inviter.verify(&invite.signing_message(), &sig) {
            return Err(TicketError::BadSignature);
        }
        if invite.expires <= now {
            return Err(TicketError::Expired);
        }
        if invite.peers.is_empty() {
            return Err(TicketError::NoPeers);
        }
        Ok(invite)
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    fn invite(sk: &SecretKey) -> Invite {
        let addr: SocketAddr = "192.168.1.20:4919".parse().unwrap();
        Invite {
            version: 1,
            repo: RepoId::from_bytes([3; 32]),
            name: "demo".into(),
            inviter: NodeId::of(sk),
            role: Role::Contributor,
            expires: 1_000,
            peers: vec![EndpointAddr::new(sk.public()).with_ip_addr(addr)],
        }
    }

    #[test]
    fn roundtrip() {
        let sk = SecretKey::generate();
        let inv = invite(&sk);
        let ticket = inv.encode(&sk);
        assert!(ticket.starts_with(TICKET_PREFIX));
        assert_eq!(Invite::decode(&ticket, 999).unwrap(), inv);
    }

    #[test]
    fn expired() {
        let sk = SecretKey::generate();
        let ticket = invite(&sk).encode(&sk);
        assert!(matches!(Invite::decode(&ticket, 1_000), Err(TicketError::Expired)));
    }

    #[test]
    fn forged_inviter() {
        let sk = SecretKey::generate();
        let mut inv = invite(&sk);
        inv.inviter = NodeId::of(&SecretKey::generate());
        let ticket = inv.encode(&sk);
        assert!(matches!(Invite::decode(&ticket, 0), Err(TicketError::BadSignature)));
    }

    #[test]
    fn garbage() {
        assert!(Invite::decode("og://join/!!!", 0).is_err());
        assert!(Invite::decode("", 0).is_err());
    }
}
