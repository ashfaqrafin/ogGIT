//! Talking to peers: serving and running `og/fetch/1`, and sending and
//! handling `og/announce/1`.
//!
//! Replication is announce, then pull. A push never sends data to anyone: the
//! owner announces a new sequence number and followers fetch from whoever
//! announced it, then re-announce so the update spreads.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use iroh::{
    EndpointAddr,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use og_core::{NodeId, RepoId};
use og_ctl::SyncReply;
use og_net::{ANNOUNCE_ALPN, Announcement, ErrorCode, FETCH_ALPN, FetchRequest, Response, read_frame, write_frame};
use og_store::{Store, SyncReport};
use tokio::{net::UnixListener, task::JoinSet, time::timeout};
use tracing::{debug, info, warn};

use crate::{Node, blocking};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const FETCH_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const LINGER: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub(crate) struct FetchProtocol(pub Arc<Node>);

impl ProtocolHandler for FetchProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        if let Err(e) = self.0.serve_fetch(&conn).await {
            warn!(peer = %NodeId::from(conn.remote_id()).short(), "fetch: {e:#}");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AnnounceProtocol(pub Arc<Node>);

impl ProtocolHandler for AnnounceProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        if let Err(e) = self.0.serve_announce(&conn).await {
            debug!(peer = %NodeId::from(conn.remote_id()).short(), "announce: {e:#}");
        }
        Ok(())
    }
}

/// Builds an `ext::` URL that makes Git run `og __pipe <socket>` as its
/// transport. In `ext::` URLs `%` escapes, and an escaped space is literal.
fn ext_url(og: &Path, socket: &Path) -> String {
    let escape = |p: &Path| p.to_string_lossy().replace('%', "%%").replace(' ', "% ");
    format!("ext::{} __pipe {}", escape(og), escape(socket))
}

impl Node {
    // ---- serving ---------------------------------------------------------

    async fn serve_fetch(self: &Arc<Self>, conn: &Connection) -> Result<()> {
        let remote = NodeId::from(conn.remote_id());
        let (mut send, mut recv) = conn.accept_bi().await?;
        let request: FetchRequest = timeout(HEADER_TIMEOUT, read_frame(&mut recv)).await??;
        let repo = request.repo;

        let store_path = match self.check_fetch(repo, remote).await {
            Ok(path) => path,
            Err((code, message)) => {
                write_frame(&mut send, &Response::error(code, message)).await?;
                send.finish()?;
                let _ = timeout(LINGER, conn.closed()).await;
                return Ok(());
            }
        };
        match request.from {
            Some(addr) if addr.id == conn.remote_id() => self.learn_peer(Some(repo), addr),
            _ => self.learn_peer(Some(repo), EndpointAddr::new(conn.remote_id())),
        }
        write_frame(&mut send, &Response::ok()).await?;
        debug!(peer = %remote.short(), %repo, "serving fetch");

        let mut cmd = std::process::Command::new("git");
        og_store::clean_env(&mut cmd);
        cmd.args(["upload-pack", "--strict", "--timeout=120"]).arg(&store_path);
        if let Some(namespace) = request.namespace {
            cmd.env("GIT_NAMESPACE", namespace.to_string());
        }
        let mut cmd = tokio::process::Command::from(cmd);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
        let mut child = cmd.spawn().context("cannot run git upload-pack")?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let mut stdout = child.stdout.take().expect("piped stdout");

        let upstream = tokio::spawn(async move {
            let _ = tokio::io::copy(&mut recv, &mut stdin).await;
        });
        let copied = timeout(FETCH_TIMEOUT, tokio::io::copy(&mut stdout, &mut send)).await;
        let _ = send.finish();
        let status = child.wait().await?;
        let upstream_abort = upstream.abort_handle();
        if timeout(LINGER, upstream).await.is_err() {
            upstream_abort.abort();
        }
        // Let the client read the tail of the stream and close first.
        let _ = timeout(Duration::from_secs(30), conn.closed()).await;
        match copied {
            Ok(Ok(bytes)) if status.success() => debug!(peer = %remote.short(), bytes, "fetch served"),
            Ok(Ok(_)) => bail!("upload-pack exited with {status}"),
            Ok(Err(e)) => bail!("stream error: {e}"),
            Err(_) => bail!("fetch timed out"),
        }
        Ok(())
    }

    /// Decides whether `remote` may fetch `repo`, returning the store path.
    async fn check_fetch(&self, repo: RepoId, remote: NodeId) -> Result<PathBuf, (ErrorCode, String)> {
        if !self.follows(&repo) {
            return Err((ErrorCode::NotFound, format!("not hosting {repo}")));
        }
        let storage = self.storage.clone();
        let checked = blocking(move || {
            let store = Store::open(&storage, repo)?;
            // A repo whose identity is not known yet has nothing to leak.
            let readable = store.identity().map(|id| id.doc.can_read(&remote)).unwrap_or(true);
            Ok((readable, store.git_dir().to_path_buf()))
        })
        .await;
        match checked {
            Ok((true, path)) => Ok(path),
            Ok((false, _)) => Err((ErrorCode::Forbidden, format!("{repo} is private"))),
            Err(e) => Err((ErrorCode::Internal, format!("{e:#}"))),
        }
    }

    async fn serve_announce(self: &Arc<Self>, conn: &Connection) -> Result<()> {
        let remote = NodeId::from(conn.remote_id());
        let (mut send, mut recv) = conn.accept_bi().await?;
        let message: Announcement = timeout(HEADER_TIMEOUT, read_frame(&mut recv)).await??;
        write_frame(&mut send, &Response::ok()).await?;
        send.finish()?;

        let Announcement::RefsUpdated { repo, node, seq, from } = message;
        let from = from.filter(|a| a.id == conn.remote_id());
        let this = self.clone();
        tokio::spawn(async move { this.on_refs_updated(repo, node, seq, remote, from).await });
        let _ = timeout(LINGER, conn.closed()).await;
        Ok(())
    }

    async fn on_refs_updated(
        self: Arc<Self>,
        repo: RepoId,
        node: NodeId,
        seq: u64,
        sender: NodeId,
        from: Option<EndpointAddr>,
    ) {
        if !self.follows(&repo) {
            return;
        }
        self.learn_peer(Some(repo), from.unwrap_or_else(|| EndpointAddr::new(sender.public_key())));
        if !self.mark_seen(repo, node, seq) {
            return;
        }
        let storage = self.storage.clone();
        let have = blocking(move || Ok(Store::open(&storage, repo)?.sigrefs(&node)?.map_or(0, |(_, s)| s.seq)))
            .await
            .unwrap_or(0);
        if have >= seq {
            return;
        }
        match self.fetch_from(repo, sender).await {
            Ok(report) => {
                log_report(&repo, sender, &report);
                for (updated, new_seq) in report.updated {
                    if updated == node || self.mark_seen(repo, updated, new_seq) {
                        self.announce(repo, updated, new_seq, Some(sender)).await;
                    }
                }
            }
            Err(e) => warn!(%repo, peer = %sender.short(), "fetch after announcement failed: {e:#}"),
        }
    }

    // ---- client side -----------------------------------------------------

    /// Fetches every namespace `peer` holds for `repo` and applies the ones
    /// that verify.
    pub(crate) async fn fetch_from(self: &Arc<Self>, repo: RepoId, peer: NodeId) -> Result<SyncReport> {
        let conn = timeout(CONNECT_TIMEOUT, self.endpoint.connect(EndpointAddr::new(peer.public_key()), FETCH_ALPN))
            .await
            .context("timed out connecting")??;
        let (mut send, mut recv) = conn.open_bi().await?;
        let request = FetchRequest { repo, namespace: None, from: Some(self.my_addr()) };
        write_frame(&mut send, &request).await?;
        let response: Response = timeout(HEADER_TIMEOUT, read_frame(&mut recv)).await.context("no reply")??;
        if !response.ok {
            bail!(
                "refused ({:?}): {}",
                response.error.unwrap_or(ErrorCode::Internal),
                response.message.unwrap_or_default()
            );
        }

        // Git connects to a private Unix socket through `og __pipe`, and we
        // splice that socket onto the QUIC stream.
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let socket = self.run_dir.join(format!("fetch-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        let bridge = tokio::spawn(async move {
            let (mut unix, _) = listener.accept().await?;
            debug!("bridge: git connected");
            let mut quic = tokio::io::join(recv, send);
            let (up, down) = tokio::io::copy_bidirectional(&mut unix, &mut quic).await?;
            debug!(up, down, "bridge: done");
            anyhow::Ok(())
        });

        let store = self.store(repo)?;
        let url = ext_url(&self.og_exe, &socket);
        let fetched = timeout(FETCH_TIMEOUT, blocking(move || Ok((store.fetch_quarantined(&url, None)?, store)))).await;

        let bridge_abort = bridge.abort_handle();
        if timeout(LINGER, bridge).await.is_err() {
            bridge_abort.abort();
        }
        let _ = std::fs::remove_file(&socket);
        conn.close(0u32.into(), b"done");

        let (quarantine, store) = fetched.context("fetch timed out")??;
        let _guard = self.repo_lock(repo).await;
        blocking(move || store.apply(quarantine)).await
    }

    /// Pulls from every known peer of `repo` at once.
    pub(crate) async fn sync(self: &Arc<Self>, repo: RepoId) -> SyncReply {
        let peers = self.repo_peers(&repo);
        let mut reply = SyncReply { peers: peers.len(), ..Default::default() };
        let mut tasks = JoinSet::new();
        for peer in peers {
            let node = self.clone();
            tasks.spawn(async move { (peer, node.fetch_from(repo, peer).await) });
        }
        let mut fresh = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            let Ok((peer, result)) = joined else { continue };
            match result {
                Ok(report) => {
                    log_report(&repo, peer, &report);
                    reply.errors.extend(report.rejected.iter().map(|r| format!("{}: rejected {r}", peer.short())));
                    for (node, seq) in report.updated {
                        reply.updated.push((node, seq));
                        fresh.push((node, seq, peer));
                    }
                }
                Err(e) => reply.errors.push(format!("{}: {e:#}", peer.short())),
            }
        }
        for (node, seq, from) in fresh {
            if self.mark_seen(repo, node, seq) {
                let this = self.clone();
                tokio::spawn(async move { this.announce(repo, node, seq, Some(from)).await });
            }
        }
        reply
    }

    /// Tells every peer of `repo` (except `skip`) that we hold `node`'s refs
    /// at `seq`.
    pub(crate) async fn announce(self: &Arc<Self>, repo: RepoId, node: NodeId, seq: u64, skip: Option<NodeId>) {
        let message = Announcement::RefsUpdated { repo, node, seq, from: Some(self.my_addr()) };
        let mut tasks = JoinSet::new();
        for peer in self.repo_peers(&repo) {
            if Some(peer) == skip || peer == node {
                continue;
            }
            let endpoint = self.endpoint.clone();
            let message = message.clone();
            tasks.spawn(async move {
                let sent = timeout(CONNECT_TIMEOUT * 2, async {
                    let conn = endpoint.connect(EndpointAddr::new(peer.public_key()), ANNOUNCE_ALPN).await?;
                    let (mut send, mut recv) = conn.open_bi().await?;
                    write_frame(&mut send, &message).await?;
                    send.finish()?;
                    let _: Response = read_frame(&mut recv).await?;
                    conn.close(0u32.into(), b"done");
                    anyhow::Ok(())
                })
                .await;
                match sent {
                    Ok(Ok(())) => debug!(peer = %peer.short(), "announced"),
                    Ok(Err(e)) => debug!(peer = %peer.short(), "announce failed: {e:#}"),
                    Err(_) => debug!(peer = %peer.short(), "announce timed out"),
                }
            });
        }
        while tasks.join_next().await.is_some() {}
    }
}

fn log_report(repo: &RepoId, peer: NodeId, report: &SyncReport) {
    for (node, seq) in &report.updated {
        info!(%repo, from = %peer.short(), node = %node.short(), seq, "updated namespace");
    }
    for rejected in &report.rejected {
        warn!(%repo, from = %peer.short(), "rejected {rejected}");
    }
}
