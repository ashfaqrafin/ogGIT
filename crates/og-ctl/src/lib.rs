//! The local control protocol (`og/ctl/1`): JSON lines over a Unix socket.
//! The `og` CLI and `git-remote-p2p` are clients; the daemon is the server.
//! Keeping the daemon long-running means `git push` never pays for network
//! setup.

use std::{
    env, fs,
    io::{self, BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use og_core::{NodeId, Oid, PushUpdate, Quorum, RepoId, Role, ticket::EndpointAddr};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// The node's home directory: `$OG_HOME`, or `~/.og`.
pub fn og_home() -> PathBuf {
    if let Some(home) = env::var_os("OG_HOME") {
        return PathBuf::from(home);
    }
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".og")
}

/// Directory for sockets. Unix socket paths are limited to about 100 bytes,
/// so a deep home directory falls back to the runtime dir.
pub fn run_dir(home: &Path) -> PathBuf {
    let local = home.join("run");
    if local.as_os_str().len() <= 80 {
        return local;
    }
    let base = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(env::temp_dir);
    let hash = blake3::hash(home.as_os_str().as_encoded_bytes()).to_hex();
    base.join(format!("og-{}", &hash[..16]))
}

pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

pub fn socket_path(home: &Path) -> PathBuf {
    run_dir(home).join("og.sock")
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Init {
        name: String,
        description: String,
        default_branch: String,
        /// Delegates besides this node.
        delegates: Vec<NodeId>,
        /// Defaults to a simple majority.
        threshold: Option<usize>,
        private: bool,
    },
    ListRefs { repo: RepoId, for_push: bool },
    Sync { repo: RepoId },
    Push { repo: RepoId, git_dir: PathBuf, updates: Vec<PushUpdate> },
    Invite { repo: RepoId, role: Role, ttl_secs: u64 },
    Join { ticket: String },
    Follow { repo: RepoId, peers: Vec<NodeId> },
    Canonical { repo: RepoId },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub data: serde_json::Value,
}

impl Reply {
    pub fn ok<T: Serialize>(data: &T) -> Self {
        match serde_json::to_value(data) {
            Ok(data) => Self { ok: true, error: None, data },
            Err(e) => Self::err(format!("failed to encode reply: {e}")),
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self { ok: false, error: Some(message.into()), data: serde_json::Value::Null }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusReply {
    pub node: NodeId,
    pub addr: EndpointAddr,
    pub home: PathBuf,
    pub repos: Vec<RepoStatus>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RepoStatus {
    pub repo: RepoId,
    pub name: Option<String>,
    pub peers: Vec<NodeId>,
    /// Each namespace held, with its signed sequence number.
    pub namespaces: Vec<(NodeId, u64)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InitReply {
    pub repo: RepoId,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RefList {
    pub refs: Vec<(Oid, String)>,
    pub head: Option<String>,
    /// The daemon's bare store, which the helper fetches objects from.
    pub store: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PushReply {
    pub results: Vec<(String, Option<String>)>,
    pub seq: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SyncReply {
    pub peers: usize,
    pub updated: Vec<(NodeId, u64)>,
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InviteReply {
    pub ticket: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JoinReply {
    pub repo: RepoId,
    pub name: String,
    pub sync: SyncReply,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CanonicalReply {
    pub name: String,
    pub default_branch: String,
    pub delegates: Vec<NodeId>,
    pub threshold: usize,
    pub refs: Vec<CanonicalRef>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CanonicalRef {
    pub name: String,
    pub tips: Vec<(NodeId, Oid)>,
    pub quorum: Quorum,
}

/// A blocking client, used by the CLI and the remote helper.
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    pub fn connect(home: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(socket_path(home))?;
        Ok(Self { reader: BufReader::new(stream.try_clone()?), writer: stream })
    }

    /// Connects to the daemon, starting it in the background first if it is
    /// not running.
    pub fn connect_or_spawn(home: &Path) -> Result<Self> {
        if let Ok(client) = Self::connect(home) {
            return Ok(client);
        }
        let og = og_binary();
        fs::create_dir_all(home)?;
        let log = fs::OpenOptions::new().create(true).append(true).open(home.join("daemon.log"))?;
        let mut cmd = Command::new(&og);
        cmd.arg("daemon")
            .env("OG_HOME", home)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            // Its own process group, so Ctrl-C on a `git` command does not stop it.
            .process_group(0);
        // Variables git set for the helper must not leak into the daemon.
        for (key, _) in env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(key);
            }
        }
        let mut child = cmd.spawn().with_context(|| format!("failed to start {} daemon", og.display()))?;

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(client) = Self::connect(home) {
                return Ok(client);
            }
            if let Some(status) = child.try_wait()? {
                bail!("the og daemon exited ({status}); see {}", home.join("daemon.log").display());
            }
            if Instant::now() > deadline {
                bail!("timed out waiting for the og daemon to start");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn call<T: DeserializeOwned>(&mut self, request: &Request) -> Result<T> {
        let mut line = serde_json::to_string(request)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).context("lost connection to the og daemon")?;
        let mut response = String::new();
        if self.reader.read_line(&mut response)? == 0 {
            bail!("the og daemon closed the connection");
        }
        let reply: Reply = serde_json::from_str(&response).context("bad reply from the og daemon")?;
        if !reply.ok {
            return Err(anyhow!(reply.error.unwrap_or_else(|| "unknown error".into())));
        }
        Ok(serde_json::from_value(reply.data)?)
    }
}

/// The `og` binary: `$OG_BIN`, a sibling of the running executable, or the
/// one on `PATH`.
pub fn og_binary() -> PathBuf {
    if let Some(bin) = env::var_os("OG_BIN") {
        return PathBuf::from(bin);
    }
    if let Ok(exe) = env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("og");
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("og")
}
