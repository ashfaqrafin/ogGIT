//! The ogGIT daemon. It owns the stores, keeps an Iroh endpoint warm, serves
//! the wire protocols to peers and the control protocol to the local `og`
//! CLI and `git-remote-p2p`.

mod ctl;
mod peer;
mod state;

use std::{
    collections::{HashMap, HashSet},
    fmt, fs,
    io::{ErrorKind, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicU64},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use iroh::{Endpoint, EndpointAddr, address_lookup::MemoryLookup, protocol::Router};
use og_core::{
    NodeId, RepoId, SecretKey,
    keys::{secret_from_hex, secret_to_hex},
};
use og_net::{ANNOUNCE_ALPN, FETCH_ALPN, Network};
use og_store::Store;
use tokio::{
    net::UnixListener,
    signal::unix::{SignalKind, signal},
    sync::{Notify, OwnedMutexGuard},
};
use tracing::info;

use crate::state::State;

pub struct Config {
    pub home: PathBuf,
    pub network: Network,
    pub port: Option<u16>,
    /// How often to pull from every peer of every followed repo, as a
    /// backstop for missed announcements.
    pub sync_interval: Duration,
}

pub async fn run(config: Config) -> Result<()> {
    let home = config.home;
    og_ctl::ensure_private_dir(&home)?;
    let run_dir = og_ctl::run_dir(&home);
    og_ctl::ensure_private_dir(&run_dir)?;

    // Only one daemon per home directory.
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(home.join("daemon.lock"))?;
    if lock.try_lock().is_err() {
        bail!("an og daemon is already running for {}", home.display());
    }
    let socket = og_ctl::socket_path(&home);
    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| format!("cannot listen on {}", socket.display()))?;

    let secret = load_or_create_key(&home)?;
    let state = State::load(&home)?;
    let lookup = MemoryLookup::new();
    for addr in state.peers.values() {
        lookup.add_endpoint_info(addr.clone());
    }
    let endpoint = og_net::bind(secret.clone(), config.network, config.port, lookup.clone()).await?;

    let node = Arc::new(Node {
        id: NodeId::of(&secret),
        secret,
        storage: home.join("storage"),
        home: home.clone(),
        run_dir,
        // The daemon is the `og` binary; Git runs `og __pipe` to reach peers.
        og_exe: match std::env::var_os("OG_BIN") {
            Some(bin) => bin.into(),
            None => std::env::current_exe()?,
        },
        endpoint: endpoint.clone(),
        relays: config.network == Network::N0,
        lookup,
        state: Mutex::new(state),
        locks: Mutex::default(),
        seen: Mutex::default(),
        shutdown: Notify::new(),
        counter: AtomicU64::new(0),
    });
    let router = Router::builder(endpoint.clone())
        .accept(FETCH_ALPN, peer::FetchProtocol(node.clone()))
        .accept(ANNOUNCE_ALPN, peer::AnnounceProtocol(node.clone()))
        .spawn();

    info!(node = %node.id, network = ?config.network, "ogGIT node started");
    tokio::spawn(node.clone().sync_loop(config.sync_interval));

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = ctl::serve(node.clone(), listener) => result?,
        _ = node.shutdown.notified() => {}
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
    info!("shutting down");
    let _ = fs::remove_file(&socket);
    let _ = router.shutdown().await;
    endpoint.close().await;
    Ok(())
}

fn load_or_create_key(home: &Path) -> Result<SecretKey> {
    let path = home.join("node.key");
    match fs::read_to_string(&path) {
        Ok(text) => secret_from_hex(&text).ok_or_else(|| anyhow!("{} is not a valid key", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let secret = SecretKey::generate();
            let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
            writeln!(file, "{}", secret_to_hex(&secret))?;
            info!(node = %NodeId::of(&secret), "created a new node key");
            Ok(secret)
        }
        Err(e) => Err(e.into()),
    }
}

pub(crate) struct Node {
    id: NodeId,
    secret: SecretKey,
    home: PathBuf,
    storage: PathBuf,
    run_dir: PathBuf,
    og_exe: PathBuf,
    endpoint: Endpoint,
    /// Whether the endpoint uses relays, i.e. can be reached across NATs.
    relays: bool,
    lookup: MemoryLookup,
    state: Mutex<State>,
    /// Serializes changes to each store.
    locks: Mutex<HashMap<RepoId, Arc<tokio::sync::Mutex<()>>>>,
    /// Announcements already handled, so floods stop.
    seen: Mutex<HashSet<(RepoId, NodeId, u64)>>,
    shutdown: Notify,
    counter: AtomicU64,
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node").field("id", &self.id).finish_non_exhaustive()
    }
}

impl Node {
    fn store(&self, repo: RepoId) -> Result<Store> {
        Store::open(&self.storage, repo)
    }

    /// Our address as peers should dial it. Loopback addresses are added so
    /// two nodes on one machine can reach each other without a LAN.
    fn my_addr(&self) -> EndpointAddr {
        let mut addr = self.endpoint.addr();
        for socket in self.endpoint.bound_sockets() {
            let ip = match socket.ip() {
                IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
                ip => ip,
            };
            addr = addr.with_ip_addr(SocketAddr::new(ip, socket.port()));
        }
        addr
    }

    fn follows(&self, repo: &RepoId) -> bool {
        self.state.lock().unwrap().repos.contains_key(repo)
    }

    fn followed(&self) -> Vec<RepoId> {
        self.state.lock().unwrap().repos.keys().copied().collect()
    }

    fn repo_peers(&self, repo: &RepoId) -> Vec<NodeId> {
        let state = self.state.lock().unwrap();
        state.repos.get(repo).map(|r| r.peers.iter().copied().collect()).unwrap_or_default()
    }

    fn follow(&self, repo: RepoId, peers: impl IntoIterator<Item = NodeId>) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let entry = state.repos.entry(repo).or_default();
        entry.peers.extend(peers.into_iter().filter(|p| *p != self.id));
        state.save(&self.home)
    }

    /// Records how to reach a peer, and that it holds `repo` if given.
    fn learn_peer(&self, repo: Option<RepoId>, addr: EndpointAddr) {
        let peer = NodeId::from(addr.id);
        if peer == self.id {
            return;
        }
        if !addr.addrs.is_empty() {
            self.lookup.add_endpoint_info(addr.clone());
        }
        let mut state = self.state.lock().unwrap();
        if !addr.addrs.is_empty() {
            state.peers.insert(peer, addr);
        }
        if let Some(entry) = repo.and_then(|r| state.repos.get_mut(&r)) {
            entry.peers.insert(peer);
        }
        if let Err(e) = state.save(&self.home) {
            tracing::warn!("cannot save state: {e:#}");
        }
    }

    /// Returns false if this announcement was already handled.
    fn mark_seen(&self, repo: RepoId, node: NodeId, seq: u64) -> bool {
        let mut seen = self.seen.lock().unwrap();
        if seen.len() > 100_000 {
            seen.clear();
        }
        seen.insert((repo, node, seq))
    }

    async fn repo_lock(&self, repo: RepoId) -> OwnedMutexGuard<()> {
        let lock = self.locks.lock().unwrap().entry(repo).or_default().clone();
        lock.lock_owned().await
    }

    async fn sync_loop(self: Arc<Self>, every: Duration) {
        tokio::time::sleep(Duration::from_secs(2)).await;
        loop {
            for repo in self.followed() {
                let reply = self.sync(repo).await;
                if !reply.updated.is_empty() || !reply.errors.is_empty() {
                    tracing::debug!(%repo, ?reply, "periodic sync");
                }
            }
            tokio::time::sleep(every).await;
        }
    }
}

/// Runs blocking store work off the async threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await?
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
