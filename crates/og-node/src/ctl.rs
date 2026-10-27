//! The control socket: one JSON request per line, one JSON reply per line.

use std::{fs, sync::Arc, time::Duration};

use anyhow::{Result, bail};
use og_core::{Doc, Invite, NodeId, Visibility};
use og_ctl::{
    CanonicalRef, CanonicalReply, InitReply, InviteReply, JoinReply, PushReply, RefList, RepoStatus, Reply,
    Request, StatusReply,
};
use og_store::Store;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

use crate::{Node, blocking, now};

pub(crate) async fn serve(node: Arc<Node>, listener: UnixListener) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let node = node.clone();
        tokio::spawn(async move {
            if let Err(e) = connection(node, stream).await {
                tracing::debug!("control connection: {e:#}");
            }
        });
    }
}

async fn connection(node: Arc<Node>, stream: UnixStream) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(request) => node.handle(request).await.unwrap_or_else(|e| Reply::err(format!("{e:#}"))),
            Err(e) => Reply::err(format!("bad request: {e}")),
        };
        let mut out = serde_json::to_vec(&reply)?;
        out.push(b'\n');
        writer.write_all(&out).await?;
    }
    Ok(())
}

impl Node {
    async fn handle(self: &Arc<Self>, request: Request) -> Result<Reply> {
        match request {
            Request::Status => self.status().await,

            Request::Init { name, description, default_branch, delegates, threshold, private } => {
                let mut all = vec![self.id];
                all.extend(delegates.into_iter().filter(|d| *d != self.id));
                let threshold = threshold.unwrap_or(all.len() / 2 + 1);
                let mut doc = Doc::new(&name, &default_branch, all, threshold);
                doc.description = description;
                if private {
                    doc.visibility = Visibility::Private;
                }
                let (storage, secret) = (self.storage.clone(), self.secret.clone());
                let repo = blocking(move || Ok(Store::init(&storage, &secret, doc)?.repo())).await?;
                self.follow(repo, [])?;
                Ok(Reply::ok(&InitReply { repo }))
            }

            Request::ListRefs { repo, for_push } => {
                let store = self.store(repo)?;
                let me = self.id;
                let list = blocking(move || {
                    let path = fs::canonicalize(store.git_dir())?;
                    if for_push {
                        return Ok(RefList { refs: store.own_refs(&me)?, head: None, store: path });
                    }
                    let advertised = store.advertised()?;
                    Ok(RefList { refs: advertised.refs, head: advertised.head, store: path })
                })
                .await?;
                Ok(Reply::ok(&list))
            }

            Request::Sync { repo } => {
                if !self.follows(&repo) {
                    bail!("not following {repo}; use `og join <ticket>` or `og follow {repo} --peer <node>`");
                }
                Ok(Reply::ok(&self.sync(repo).await))
            }

            Request::Push { repo, git_dir, updates } => {
                let guard = self.repo_lock(repo).await;
                let store = self.store(repo)?;
                let secret = self.secret.clone();
                let outcome = blocking(move || store.push(&secret, &git_dir, &updates)).await?;
                drop(guard);
                if let Some(seq) = outcome.seq {
                    self.mark_seen(repo, self.id, seq);
                    let this = self.clone();
                    tokio::spawn(async move { this.announce(repo, this.id, seq, None).await });
                }
                Ok(Reply::ok(&PushReply { results: outcome.results, seq: outcome.seq }))
            }

            Request::Invite { repo, role, ttl_secs } => {
                let store = self.store(repo)?;
                let name = blocking(move || Ok(store.identity()?.doc.name)).await?;
                if self.relays {
                    // A freshly started daemon may not have a home relay yet,
                    // and the ticket should carry it for peers behind NAT.
                    let _ = tokio::time::timeout(Duration::from_secs(5), self.endpoint.online()).await;
                }
                let mut peers = vec![self.my_addr()];
                {
                    // A few other holders as seed hints, in case we are offline.
                    let state = self.state.lock().unwrap();
                    let holders = state.repos.get(&repo).map(|r| r.peers.iter()).into_iter().flatten();
                    peers.extend(holders.filter_map(|p| state.peers.get(p).cloned()).take(3));
                }
                let invite = Invite { version: 1, repo, name, inviter: self.id, role, expires: now() + ttl_secs, peers };
                Ok(Reply::ok(&InviteReply { ticket: invite.encode(&self.secret) }))
            }

            Request::Join { ticket } => {
                let invite = Invite::decode(&ticket, now())?;
                let repo = invite.repo;
                for addr in &invite.peers {
                    self.learn_peer(None, addr.clone());
                }
                let peers = invite.peers.iter().map(|a| NodeId::from(a.id));
                self.follow(repo, peers)?;
                self.join_repo(repo).await
            }

            Request::Follow { repo, peers } => {
                self.follow(repo, peers)?;
                self.join_repo(repo).await
            }

            Request::Canonical { repo } => {
                let store = self.store(repo)?;
                let view = blocking(move || store.canonical()).await?;
                let doc = view.identity.doc;
                let refs = view
                    .branches
                    .into_iter()
                    .chain(view.tags)
                    .map(|r| CanonicalRef { name: r.name, tips: r.tips.into_iter().collect(), quorum: r.quorum })
                    .collect();
                Ok(Reply::ok(&CanonicalReply {
                    name: doc.name,
                    default_branch: doc.default_branch,
                    delegates: doc.delegates,
                    threshold: doc.threshold,
                    refs,
                }))
            }

            Request::Shutdown => {
                let this = self.clone();
                tokio::spawn(async move {
                    // Let the reply reach the client first.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    this.shutdown.notify_one();
                });
                Ok(Reply::ok(&()))
            }
        }
    }

    /// Creates the store for a newly followed repo and pulls it from peers.
    async fn join_repo(self: &Arc<Self>, repo: og_core::RepoId) -> Result<Reply> {
        let storage = self.storage.clone();
        blocking(move || Store::open_or_create(&storage, repo).map(drop)).await?;
        let sync = self.sync(repo).await;
        let store = self.store(repo)?;
        let name = match blocking(move || Ok(store.identity()?.doc.name)).await {
            Ok(name) => name,
            Err(e) if sync.errors.is_empty() => bail!("could not fetch {repo}: {e:#}"),
            Err(_) => bail!("could not fetch {repo}: {}", sync.errors.join("; ")),
        };
        Ok(Reply::ok(&JoinReply { repo, name, sync }))
    }

    async fn status(self: &Arc<Self>) -> Result<Reply> {
        let mut repos = Vec::new();
        for repo in self.followed() {
            let peers = self.repo_peers(&repo);
            let storage = self.storage.clone();
            let (name, namespaces) = blocking(move || {
                let store = Store::open(&storage, repo)?;
                let name = store.identity().ok().map(|i| i.doc.name);
                let mut namespaces = Vec::new();
                for node in store.namespaces()?.into_keys() {
                    if let Ok(Some((_, sigrefs))) = store.sigrefs(&node) {
                        namespaces.push((node, sigrefs.seq));
                    }
                }
                Ok((name, namespaces))
            })
            .await
            .unwrap_or((None, Vec::new()));
            repos.push(RepoStatus { repo, name, peers, namespaces });
        }
        Ok(Reply::ok(&StatusReply { node: self.id, addr: self.my_addr(), home: self.home.clone(), repos }))
    }
}
