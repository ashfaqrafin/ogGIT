//! `git-remote-p2p`: Git runs this for any remote URL starting with `p2p://`.
//! It speaks Git's remote-helper protocol on stdin/stdout and hands each
//! command to the local og daemon, so plain `git clone`, `fetch` and `push`
//! work with the network. See `git help gitremote-helpers`.

use std::{
    env, fs,
    io::{self, BufRead, Write},
    path::PathBuf,
    process::{Command, ExitCode, Stdio},
};

use anyhow::{Context, Result, bail};
use og_core::{Oid, PushUpdate, RepoId};
use og_ctl::{Client, PushReply, RefList, Request, SyncReply};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("git-remote-p2p: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn parse_url(url: &str) -> Result<RepoId> {
    let id = url.strip_prefix("p2p://").unwrap_or(url).trim_end_matches('/');
    id.parse().with_context(|| format!("{url} is not a p2p://og:<id> URL"))
}

struct Helper {
    repo: RepoId,
    git_dir: PathBuf,
    daemon: Client,
    store: Option<PathBuf>,
    verbosity: u32,
}

fn run() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let url = args.get(2).or(args.get(1)).context("usage: git-remote-p2p <remote> <url>")?;
    let repo = parse_url(url)?;
    let git_dir = env::var_os("GIT_DIR").context("GIT_DIR is not set; this helper is run by git")?;
    let git_dir = fs::canonicalize(git_dir)?;
    let daemon = Client::connect_or_spawn(&og_ctl::og_home())?;
    let mut helper = Helper { repo, git_dir, daemon, store: None, verbosity: 1 };

    let mut lines = io::stdin().lock().lines();
    let mut out = io::stdout().lock();
    while let Some(line) = lines.next() {
        let line = line?;
        let (command, arg) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        match command {
            "capabilities" => write!(out, "fetch\npush\noption\n\n")?,
            "option" => writeln!(out, "{}", helper.option(arg))?,
            "list" => {
                for entry in helper.list(arg == "for-push")? {
                    writeln!(out, "{entry}")?;
                }
                writeln!(out)?;
            }
            // `fetch` and `push` arrive in batches ended by a blank line.
            "fetch" | "push" => {
                let mut batch = vec![arg.to_string()];
                for next in lines.by_ref() {
                    let next = next?;
                    if next.is_empty() {
                        break;
                    }
                    let (_, rest) = next.split_once(' ').unwrap_or(("", &next));
                    batch.push(rest.to_string());
                }
                if command == "fetch" {
                    helper.fetch(&batch)?;
                } else {
                    for status in helper.push(&batch)? {
                        writeln!(out, "{status}")?;
                    }
                }
                writeln!(out)?;
            }
            "" => break,
            other => bail!("unsupported command {other:?}"),
        }
        out.flush()?;
    }
    Ok(())
}

impl Helper {
    fn option(&mut self, arg: &str) -> &'static str {
        match arg.split_once(' ') {
            Some(("verbosity", n)) => {
                self.verbosity = n.parse().unwrap_or(1);
                "ok"
            }
            Some(("progress", _)) => "ok",
            _ => "unsupported",
        }
    }

    fn list(&mut self, for_push: bool) -> Result<Vec<String>> {
        if !for_push {
            // Pull the latest from peers first. Failures only matter if we
            // have nothing at all, and then ListRefs reports it.
            match self.daemon.call::<SyncReply>(&Request::Sync { repo: self.repo }) {
                Ok(reply) if self.verbosity > 0 => {
                    for error in reply.errors {
                        eprintln!("og: warning: {error}");
                    }
                }
                Ok(_) => {}
                Err(e) => eprintln!("og: warning: {e:#}"),
            }
        }
        let list: RefList = self.daemon.call(&Request::ListRefs { repo: self.repo, for_push })?;
        let mut lines: Vec<String> = list.refs.iter().map(|(oid, name)| format!("{oid} {name}")).collect();
        if let Some(head) = list.head {
            lines.push(format!("@{head} HEAD"));
        }
        self.store = Some(list.store);
        Ok(lines)
    }

    /// Copies the wanted objects from the daemon's store into this repo.
    /// Git updates the refs itself afterwards.
    fn fetch(&mut self, batch: &[String]) -> Result<()> {
        let store = match &self.store {
            Some(store) => store.clone(),
            None => self.daemon.call::<RefList>(&Request::ListRefs { repo: self.repo, for_push: false })?.store,
        };
        let mut wants: Vec<&str> = batch.iter().filter_map(|l| l.split(' ').next()).collect();
        wants.sort();
        wants.dedup();
        let mut child = Command::new("git")
            .env("GIT_DIR", &self.git_dir)
            .args(["-c", "protocol.version=2", "-c", "gc.auto=0", "-c", "maintenance.auto=false"])
            .args(["fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "--no-recurse-submodules", "--stdin"])
            .arg(&store)
            .stdin(Stdio::piped())
            .spawn()
            .context("cannot run git fetch")?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        for oid in wants {
            writeln!(stdin, "{oid}")?;
        }
        drop(stdin);
        if !child.wait()?.success() {
            bail!("copying objects from the og store failed");
        }
        Ok(())
    }

    /// Hands the pushed refs to the daemon, which updates our own namespace
    /// and signs it. Nothing is sent to anyone: peers pull from us.
    fn push(&mut self, batch: &[String]) -> Result<Vec<String>> {
        let mut statuses = Vec::new();
        let mut updates = Vec::new();
        for spec in batch {
            let (force, spec) = match spec.strip_prefix('+') {
                Some(rest) => (true, rest),
                None => (false, spec.as_str()),
            };
            let Some((src, dst)) = spec.split_once(':') else {
                bail!("bad push refspec {spec:?}");
            };
            let src = if src.is_empty() {
                None
            } else {
                match self.rev_parse(src) {
                    Ok(oid) => Some(oid),
                    Err(e) => {
                        statuses.push(format!("error {dst} {}", one_line(&format!("{e:#}"))));
                        continue;
                    }
                }
            };
            updates.push(PushUpdate { src, dst: dst.to_string(), force });
        }
        if updates.is_empty() {
            return Ok(statuses);
        }
        let reply: PushReply = self.daemon.call(&Request::Push {
            repo: self.repo,
            git_dir: self.git_dir.clone(),
            updates,
        })?;
        for (dst, error) in reply.results {
            match error {
                None => statuses.push(format!("ok {dst}")),
                Some(e) => statuses.push(format!("error {dst} {}", one_line(&e))),
            }
        }
        if let (Some(seq), true) = (reply.seq, self.verbosity > 0) {
            eprintln!("og: signed refs at seq {seq}; announcing to peers");
        }
        Ok(statuses)
    }

    fn rev_parse(&self, rev: &str) -> Result<Oid> {
        let out = Command::new("git")
            .env("GIT_DIR", &self.git_dir)
            .args(["rev-parse", "--verify", "--quiet", rev])
            .stderr(Stdio::null())
            .output()?;
        if !out.status.success() {
            bail!("cannot resolve {rev}");
        }
        Ok(Oid::parse(String::from_utf8_lossy(&out.stdout).trim())?)
    }
}

fn one_line(s: &str) -> String {
    s.replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let id = RepoId::from_bytes([1; 32]);
        assert_eq!(parse_url(&format!("p2p://{id}")).unwrap(), id);
        assert_eq!(parse_url(&format!("p2p://{id}/")).unwrap(), id);
        assert_eq!(parse_url(&format!("p2p://{}", id.base32())).unwrap(), id);
        assert!(parse_url("p2p://example.com/repo").is_err());
    }
}
