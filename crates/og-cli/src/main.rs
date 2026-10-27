//! `og`: create, share and follow ogGIT repos, and run the daemon.

use std::{
    io,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use og_core::{NodeId, Quorum, RepoId, Role};
use og_ctl::{
    CanonicalReply, Client, InitReply, InviteReply, JoinReply, Request, StatusReply, SyncReply, og_home,
};
use og_net::Network;

#[derive(Parser)]
#[command(name = "og", version, about = "ogGIT: peer-to-peer Git with signed refs")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Turn the Git repo in the current directory into an ogGIT repo
    Init {
        /// Repo name (defaults to the directory name)
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "")]
        description: String,
        /// Defaults to the current branch
        #[arg(long)]
        default_branch: Option<String>,
        /// Another delegate's node ID (repeatable). You are always one.
        #[arg(long = "delegate", value_name = "NODE_ID")]
        delegates: Vec<NodeId>,
        /// Delegate signatures needed to move a canonical branch (default: majority)
        #[arg(long)]
        threshold: Option<usize>,
        /// Serve the repo only to its delegates
        #[arg(long)]
        private: bool,
        /// Name of the Git remote to add
        #[arg(long, default_value = "og")]
        remote: String,
    },
    /// Join a repo from an invite ticket and clone it
    Join {
        ticket: String,
        /// Where to clone (defaults to the repo name)
        dir: Option<PathBuf>,
    },
    /// Replicate a repo without a working copy, e.g. on a seed node
    #[command(alias = "seed")]
    Follow {
        repo: RepoId,
        /// A node that holds the repo (repeatable)
        #[arg(long = "peer", value_name = "NODE_ID")]
        peers: Vec<NodeId>,
    },
    /// Developer circle: invite collaborators
    Circle {
        #[command(subcommand)]
        command: CircleCmd,
    },
    /// Pull the latest refs from every peer now
    Sync {
        #[arg(long)]
        repo: Option<RepoId>,
    },
    /// Show how each canonical branch was decided
    Canonical {
        #[arg(long)]
        repo: Option<RepoId>,
    },
    /// Show this node and the repos it follows
    Status,
    /// Print this node's ID
    Id,
    /// Run the daemon in the foreground (it normally starts on demand)
    Daemon {
        /// "n0" uses public relays and DNS lookup; "local" uses direct addresses only
        #[arg(long, env = "OG_NETWORK", default_value = "n0")]
        network: Network,
        /// UDP port to listen on (default: random)
        #[arg(long, env = "OG_PORT")]
        port: Option<u16>,
        /// Seconds between background syncs with every peer
        #[arg(long, env = "OG_SYNC_INTERVAL", default_value_t = 300)]
        sync_interval: u64,
    },
    /// Stop the daemon
    Stop,
    /// Internal: connects Git's ext:: transport to the daemon
    #[command(name = "__pipe", hide = true)]
    Pipe { socket: PathBuf },
}

#[derive(Subcommand)]
enum CircleCmd {
    /// Print an invite link for the current repo
    Invite {
        #[arg(long)]
        repo: Option<RepoId>,
        #[arg(long, value_enum, default_value_t = RoleArg::Contributor)]
        role: RoleArg,
        /// How long the link stays valid: 30m, 12h, 7d
        #[arg(long, default_value = "7d")]
        expires: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum RoleArg {
    Contributor,
    Follower,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if !matches!(cli.command, Cmd::Daemon { .. }) {
        // Exit quietly when output is piped into something like `head`,
        // instead of panicking on a write to a closed pipe. The daemon keeps
        // Rust's default, so a vanished client is just an I/O error.
        // SAFETY: called before any other threads exist.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("og: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn daemon() -> Result<Client> {
    Client::connect_or_spawn(&og_home())
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Cmd::Init { name, description, default_branch, delegates, threshold, private, remote } => {
            let top = git(None, &["rev-parse", "--show-toplevel"]).context("run og init inside a Git repo")?;
            let name = name.unwrap_or_else(|| sanitize(Path::new(&top).file_name().unwrap_or_default().to_string_lossy().as_ref()));
            let default_branch = default_branch
                .or_else(|| git(None, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok())
                .unwrap_or_else(|| "main".into());
            let reply: InitReply = daemon()?.call(&Request::Init {
                name: name.clone(),
                description,
                default_branch: default_branch.clone(),
                delegates,
                threshold,
                private,
            })?;
            let url = format!("p2p://{}", reply.repo);
            let remote_note = if git(None, &["remote", "add", &remote, &url]).is_ok() {
                format!("{remote} -> {url}")
            } else {
                format!("not added; `{remote}` exists. Run: git remote set-url {remote} {url}")
            };
            println!("Created ogGIT repo {name}");
            println!("  id      {}", reply.repo);
            println!("  remote  {remote_note}");
            println!();
            println!("Next:");
            println!("  git push {remote} {default_branch}");
            println!("  og circle invite      # a link for collaborators");
        }

        Cmd::Join { ticket, dir } => {
            let reply: JoinReply = daemon()?.call(&Request::Join { ticket })?;
            for error in &reply.sync.errors {
                eprintln!("og: warning: {error}");
            }
            let dir = dir.unwrap_or_else(|| PathBuf::from(&reply.name));
            println!("Joined {} ({})", reply.name, reply.repo);
            let status = Command::new("git")
                .args(["clone", &format!("p2p://{}", reply.repo)])
                .arg(&dir)
                .status()?;
            if !status.success() {
                bail!("git clone failed");
            }
        }

        Cmd::Follow { repo, peers } => {
            let reply: JoinReply = daemon()?.call(&Request::Follow { repo, peers })?;
            print_sync(&reply.sync);
            println!("Following {} ({})", reply.name, reply.repo);
        }

        Cmd::Circle { command: CircleCmd::Invite { repo, role, expires } } => {
            let repo = current_repo(repo)?;
            let role = match role {
                RoleArg::Contributor => Role::Contributor,
                RoleArg::Follower => Role::Follower,
            };
            let ttl_secs = parse_duration(&expires)?;
            let reply: InviteReply = daemon()?.call(&Request::Invite { repo, role, ttl_secs })?;
            println!("{}", reply.ticket);
            eprintln!("\nShare this link. They run: og join <link>   (valid for {expires})");
        }

        Cmd::Sync { repo } => {
            let repo = current_repo(repo)?;
            let reply: SyncReply = daemon()?.call(&Request::Sync { repo })?;
            print_sync(&reply);
        }

        Cmd::Canonical { repo } => {
            let repo = current_repo(repo)?;
            let reply: CanonicalReply = daemon()?.call(&Request::Canonical { repo })?;
            print_canonical(&reply);
        }

        Cmd::Status => {
            let s: StatusReply = daemon()?.call(&Request::Status)?;
            println!("node   {}", s.node);
            println!("home   {}", s.home.display());
            let addrs: Vec<String> = s.addr.addrs.iter().map(|a| format!("{a:?}")).collect();
            println!("addrs  {}", if addrs.is_empty() { "(none yet)".into() } else { addrs.join(", ") });
            if s.repos.is_empty() {
                println!("\nNo repos yet. Run `og init` in a Git repo, or `og join <link>`.");
            }
            for repo in s.repos {
                println!("\n{}  {}", repo.name.as_deref().unwrap_or("(not fetched yet)"), repo.repo);
                let peers: Vec<String> = repo.peers.iter().map(NodeId::short).collect();
                println!("  peers       {}", if peers.is_empty() { "none".into() } else { peers.join(", ") });
                for (node, seq) in repo.namespaces {
                    let you = if node == s.node { " (you)" } else { "" };
                    println!("  namespace   {} seq {seq}{you}", node.short());
                }
            }
        }

        Cmd::Id => {
            let s: StatusReply = daemon()?.call(&Request::Status)?;
            println!("{}", s.node);
        }

        Cmd::Daemon { network, port, sync_interval } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_env("OG_LOG")
                        .unwrap_or_else(|_| "warn,og_node=info,og_store=info".into()),
                )
                .init();
            let config = og_node::Config {
                home: og_home(),
                network,
                port,
                sync_interval: Duration::from_secs(sync_interval.max(5)),
            };
            tokio::runtime::Runtime::new()?.block_on(og_node::run(config))?;
        }

        Cmd::Stop => match Client::connect(&og_home()) {
            Ok(mut client) => {
                client.call::<()>(&Request::Shutdown)?;
                wait_for_exit(&og_home())?;
                println!("og daemon stopped");
            }
            Err(_) => println!("og daemon is not running"),
        },

        Cmd::Pipe { socket } => pipe(&socket)?,
    }
    Ok(())
}

/// The daemon holds `daemon.lock` until it exits.
fn wait_for_exit(home: &Path) -> Result<()> {
    let lock = std::fs::File::open(home.join("daemon.lock"))?;
    for _ in 0..200 {
        if lock.try_lock().is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("the og daemon is taking a long time to stop")
}

fn print_sync(reply: &SyncReply) {
    for error in &reply.errors {
        eprintln!("og: warning: {error}");
    }
    if reply.updated.is_empty() {
        println!("Up to date ({} peers)", reply.peers);
    }
    for (node, seq) in &reply.updated {
        println!("Updated {} to seq {seq}", node.short());
    }
}

fn print_canonical(reply: &CanonicalReply) {
    println!(
        "{}: {} delegates, {} must agree to move a branch",
        reply.name,
        reply.delegates.len(),
        reply.threshold
    );
    if reply.refs.is_empty() {
        println!("  no branches pushed by delegates yet");
    }
    for r in &reply.refs {
        let verdict = match &r.quorum {
            Quorum::Agreed { oid, votes } => format!("{} ({votes} of {})", &oid.as_str()[..12], reply.delegates.len()),
            Quorum::TooFewDelegates { have, need } => format!("no quorum: {have} delegates have it, {need} needed"),
            Quorum::Diverged { candidates } => {
                let c: Vec<_> = candidates.iter().map(|o| &o.as_str()[..12]).collect();
                format!("no quorum: delegates diverged between {}", c.join(" and "))
            }
            Quorum::NoCommonHistory => "no quorum: delegates share no history".into(),
        };
        println!("\n{}  {verdict}", r.name);
        for (node, oid) in &r.tips {
            println!("    {}  {}", node.short(), &oid.as_str()[..12]);
        }
    }
}

/// Copies stdin to the socket and the socket to stdout. Git's `ext::`
/// transport runs this, and the daemon splices the socket onto a QUIC stream.
fn pipe(socket: &Path) -> Result<()> {
    use std::{fs::File, net::Shutdown, os::fd::AsFd};

    let stream = UnixStream::connect(socket)?;
    let from_peer = stream.try_clone()?;
    // Raw files rather than Stdout, whose line buffering would flush on every
    // newline byte in a packfile.
    let stdout = File::from(io::stdout().as_fd().try_clone_to_owned()?);
    let stdin = File::from(io::stdin().as_fd().try_clone_to_owned()?);
    let reader = std::thread::spawn(move || pump(from_peer, stdout));
    pump(stdin, &stream)?;
    stream.shutdown(Shutdown::Write)?;
    reader.join().expect("pipe thread panicked")?;
    Ok(())
}

/// A plain read/write loop. `io::copy` is not used because on Linux it
/// switches to `splice()`, and a splice blocked on stdin stalls the other
/// direction of the same socket, deadlocking Git's request/response protocol.
fn pump(mut from: impl io::Read, mut to: impl io::Write) -> io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return to.flush(),
            Ok(n) => to.write_all(&buf[..n])?,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Runs git in the current directory and returns trimmed stdout.
fn git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    let out = cmd.args(args).output()?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Finds the repo ID from this Git repo's p2p:// remote, preferring `og`,
/// then `origin`.
fn current_repo(explicit: Option<RepoId>) -> Result<RepoId> {
    if let Some(repo) = explicit {
        return Ok(repo);
    }
    let out = git(None, &["config", "--get-regexp", r"^remote\..*\.url$"]).unwrap_or_default();
    let mut found: Vec<(String, RepoId)> = out
        .lines()
        .filter_map(|line| {
            let (key, url) = line.split_once(' ')?;
            let remote = key.strip_prefix("remote.")?.strip_suffix(".url")?;
            let repo = url.strip_prefix("p2p://")?.trim_end_matches('/').parse().ok()?;
            Some((remote.to_string(), repo))
        })
        .collect();
    found.sort_by_key(|(remote, _)| match remote.as_str() {
        "og" => 0,
        "origin" => 1,
        _ => 2,
    });
    match found.first() {
        Some((_, repo)) => Ok(*repo),
        None => bail!("no p2p:// remote in this directory; pass --repo og:<id>"),
    }
}

fn sanitize(name: &str) -> String {
    let cleaned: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect();
    let cleaned = cleaned.trim_start_matches('.').to_string();
    if cleaned.is_empty() { "repo".into() } else { cleaned }
}

fn parse_duration(s: &str) -> Result<u64> {
    let (number, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = number.parse().with_context(|| format!("bad duration {s:?}"))?;
    let scale = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => bail!("bad duration {s:?}; use s, m, h or d"),
    };
    Ok(n * scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), 90);
        assert_eq!(parse_duration("30m").unwrap(), 1800);
        assert_eq!(parse_duration("7d").unwrap(), 604_800);
        assert!(parse_duration("7w").is_err());
        assert!(parse_duration("d").is_err());
    }

    #[test]
    fn names() {
        assert_eq!(sanitize("my project"), "my-project");
        assert_eq!(sanitize(".hidden"), "hidden");
        assert_eq!(sanitize(""), "repo");
    }
}
