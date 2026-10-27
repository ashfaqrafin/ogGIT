//! Wire protocols. Iroh authenticates both ends by public key, so there is
//! no custom handshake: one ALPN per job, with the version in the name.
//!
//! Every stream starts with one JSON line from the client and one JSON line
//! reply. For `og/fetch/1` the stream then carries Git's own `upload-pack`
//! protocol in both directions.

use std::{net::SocketAddr, str::FromStr};

use anyhow::{Context, Result, bail};
use iroh::{
    Endpoint, EndpointAddr, RelayMode, SecretKey,
    address_lookup::MemoryLookup,
    endpoint::presets,
};
use og_core::{NodeId, RepoId};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Repo data: a [`FetchRequest`], then Git's upload-pack protocol.
pub const FETCH_ALPN: &[u8] = b"og/fetch/1";
/// Ref update announcements.
pub const ANNOUNCE_ALPN: &[u8] = b"og/announce/1";

/// Largest JSON header accepted from a peer.
pub const MAX_FRAME: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Network {
    /// Iroh's public relays and DNS address lookup: works across NATs.
    #[default]
    N0,
    /// Direct addresses only: no relays, no lookups. For LANs and tests.
    Local,
}

impl FromStr for Network {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "n0" | "internet" => Ok(Self::N0),
            "local" => Ok(Self::Local),
            _ => bail!("unknown network {s:?}, expected \"n0\" or \"local\""),
        }
    }
}

pub async fn bind(secret: SecretKey, network: Network, port: Option<u16>, lookup: MemoryLookup) -> Result<Endpoint> {
    let builder = match network {
        Network::N0 => Endpoint::builder(presets::N0),
        Network::Local => Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled),
    };
    let mut builder = builder
        .secret_key(secret)
        .alpns(vec![FETCH_ALPN.to_vec(), ANNOUNCE_ALPN.to_vec()])
        .address_lookup(lookup);
    if let Some(port) = port {
        builder = builder
            .bind_addr(SocketAddr::from(([0, 0, 0, 0], port)))
            .context("invalid bind address")?;
    }
    builder.bind().await.context("failed to bind the network endpoint")
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FetchRequest {
    pub repo: RepoId,
    /// Fetch one peer's namespace only; `None` fetches all of them.
    #[serde(default)]
    pub namespace: Option<NodeId>,
    /// How to reach the requester, so the server can announce to it later.
    #[serde(default)]
    pub from: Option<EndpointAddr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Announcement {
    /// `node`'s signed refs for `repo` reached sequence number `seq`, and the
    /// sender holds them.
    RefsUpdated {
        repo: RepoId,
        node: NodeId,
        seq: u64,
        #[serde(default)]
        from: Option<EndpointAddr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    NotFound,
    Forbidden,
    BadRequest,
    Busy,
    Internal,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Response {
    pub fn ok() -> Self {
        Self { ok: true, error: None, message: None }
    }

    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { ok: false, error: Some(code), message: Some(message.into()) }
    }
}

/// Reads one newline-terminated JSON frame. Reads byte by byte so nothing
/// past the newline is consumed: for fetch, Git's protocol follows directly.
pub async fn read_frame<T: DeserializeOwned>(reader: &mut (impl AsyncRead + Unpin)) -> Result<T> {
    let mut buf = Vec::with_capacity(256);
    loop {
        let byte = reader.read_u8().await.context("stream ended before the header")?;
        if byte == b'\n' {
            break;
        }
        if buf.len() >= MAX_FRAME {
            bail!("header larger than {MAX_FRAME} bytes");
        }
        buf.push(byte);
    }
    serde_json::from_slice(&buf).context("malformed header")
}

pub async fn write_frame<T: Serialize>(writer: &mut (impl AsyncWrite + Unpin), value: &T) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_stop_at_the_newline() {
        let req = Response::error(ErrorCode::NotFound, "nope");
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).await.unwrap();
        buf.extend_from_slice(b"PACK...");
        let mut cursor = std::io::Cursor::new(buf);
        let back: Response = read_frame(&mut cursor).await.unwrap();
        assert_eq!(back.error, Some(ErrorCode::NotFound));
        let mut rest = Vec::new();
        cursor.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"PACK...");
    }

    #[tokio::test]
    async fn oversized_frames_are_refused() {
        let mut cursor = std::io::Cursor::new(vec![b'x'; MAX_FRAME + 10]);
        assert!(read_frame::<Response>(&mut cursor).await.is_err());
    }
}
