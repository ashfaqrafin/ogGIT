//! Storage for ogGIT: one bare Git repo per repo ID. Each peer's refs live in
//! its own namespace, `refs/namespaces/<node-id>/refs/...`, and a node only
//! ever writes its own.
//!
//! The rule for anything arriving from the network: objects land in a
//! quarantine repo first, and no ref changes until the namespace owner's
//! signature, the sequence number and object connectivity have been checked.

mod git;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, anyhow, bail};
use og_core::{
    Doc, History, IDENTITY_REF, NodeId, Oid, PushUpdate, Quorum, RepoId, Revision, SIGREFS_REF,
    SecretKey, SignedRefs,
    canonical::{branch_quorum, exact_quorum},
    identity::verify_history,
    namespace_prefix,
    refs::validate_ref_name,
    sigrefs::{MAX_MANIFEST_BYTES, Update, check_update},
};
use serde::Serialize;

pub use crate::git::{Git, clean_env};

const MAX_IDENTITY_FILE: u64 = 64 * 1024;
const MAX_IDENTITY_REVISIONS: usize = 1000;

#[derive(Debug)]
pub struct Store {
    repo: RepoId,
    git: Git,
    tmp: PathBuf,
}

/// The latest valid identity document, and which namespace it came from.
#[derive(Clone, Debug, Serialize)]
pub struct Identity {
    pub doc: Doc,
    pub revisions: usize,
    pub source: NodeId,
}

#[derive(Clone, Debug, Serialize)]
pub struct RefQuorum {
    pub name: String,
    pub tips: BTreeMap<NodeId, Oid>,
    pub quorum: Quorum,
}

#[derive(Clone, Debug, Serialize)]
pub struct CanonicalView {
    pub identity: Identity,
    pub branches: Vec<RefQuorum>,
    pub tags: Vec<RefQuorum>,
}

/// What `git fetch` sees: canonical branches and tags, plus every peer's refs
/// under `refs/peers/<node-id>/`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Advertised {
    pub refs: Vec<(Oid, String)>,
    pub head: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PushOutcome {
    /// Per destination ref: `None` on success, or the reason it was refused.
    pub results: Vec<(String, Option<String>)>,
    /// The new sigrefs sequence number, if the namespace changed.
    pub seq: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SyncReport {
    /// Namespaces that moved forward, with their new sequence number.
    pub updated: Vec<(NodeId, u64)>,
    /// Namespaces where the peer had an older manifest than ours. Common
    /// while an update is still spreading, and harmless: it is ignored.
    pub stale: Vec<NodeId>,
    /// Namespaces that failed verification, with the reason.
    pub rejected: Vec<String>,
}

/// What to do with one fetched namespace.
enum Verdict {
    Apply(Oid, Box<SignedRefs>),
    Same,
    /// Older than ours: a peer that has not caught up, or a replay. Either
    /// way it is not applied.
    Stale,
}

/// Objects fetched from a peer, held apart from the store until verified.
pub struct Quarantine {
    dir: tempfile::TempDir,
    git: Git,
}

enum RefOp {
    Update { name: String, new: Oid, old: Option<Oid> },
    Delete { name: String, old: Option<Oid> },
}

impl Store {
    pub fn path(root: &Path, repo: &RepoId) -> PathBuf {
        root.join(format!("{}.git", repo.base32()))
    }

    pub fn exists(root: &Path, repo: &RepoId) -> bool {
        Self::path(root, repo).join("HEAD").is_file()
    }

    pub fn open(root: &Path, repo: RepoId) -> Result<Self> {
        if !Self::exists(root, &repo) {
            bail!("this node does not have {repo}");
        }
        Ok(Self::at(root, repo))
    }

    pub fn open_or_create(root: &Path, repo: RepoId) -> Result<Self> {
        if Self::exists(root, &repo) {
            return Self::open(root, repo);
        }
        let path = Self::path(root, &repo);
        let status = clean_env(&mut Command::new("git"))
            .args(["init", "--bare", "--quiet"])
            .arg(&path)
            .status()
            .context("failed to run git init")?;
        if !status.success() {
            bail!("git init failed for {}", path.display());
        }
        let store = Self::at(root, repo);
        for (key, value) in [
            ("core.logAllRefUpdates", "false"),
            ("gc.auto", "0"),
            ("maintenance.auto", "false"),
            ("transfer.fsckObjects", "true"),
            ("uploadpack.allowFilter", "true"),
            ("og.repo", &repo.to_string()),
        ] {
            store.git.run(["config", key, value])?;
        }
        Ok(store)
    }

    fn at(root: &Path, repo: RepoId) -> Self {
        Self { repo, git: Git::new(Self::path(root, &repo)), tmp: root.join("tmp") }
    }

    /// Creates a new repo from its first identity revision, signed by `secret`.
    pub fn init(root: &Path, secret: &SecretKey, doc: Doc) -> Result<Self> {
        doc.validate()?;
        if !doc.is_delegate(&NodeId::of(secret)) {
            bail!("the creator must be one of the delegates");
        }
        let mut genesis = Revision::new(doc);
        genesis.sign(secret);
        let repo = genesis.repo_id();
        if Self::exists(root, &repo) {
            bail!("{repo} already exists on this node");
        }
        let store = Self::open_or_create(root, repo)?;
        store.write_identity(secret, &genesis)?;
        store.sign_refs(secret)?;
        Ok(store)
    }

    pub fn repo(&self) -> RepoId {
        self.repo
    }

    pub fn git_dir(&self) -> &Path {
        self.git.dir()
    }

    // ---- refs and objects ------------------------------------------------

    fn refs_in(git: &Git, prefix: &str) -> Result<BTreeMap<String, Oid>> {
        let out = git.run(["for-each-ref", "--format=%(objectname) %(refname)", prefix])?;
        out.lines()
            .map(|line| {
                let (oid, name) = line.split_once(' ').ok_or_else(|| anyhow!("bad for-each-ref line"))?;
                Ok((name.to_string(), Oid::parse(oid)?))
            })
            .collect()
    }

    fn namespaces_in(git: &Git) -> Result<BTreeMap<NodeId, BTreeMap<String, Oid>>> {
        let mut out: BTreeMap<NodeId, BTreeMap<String, Oid>> = BTreeMap::new();
        for (name, oid) in Self::refs_in(git, "refs/namespaces/")? {
            let rest = &name["refs/namespaces/".len()..];
            let Some((node, inner)) = rest.split_once('/') else { continue };
            let Ok(node) = node.parse::<NodeId>() else { continue };
            out.entry(node).or_default().insert(inner.to_string(), oid);
        }
        Ok(out)
    }

    /// Every namespace in the store: node → (ref inside the namespace → oid).
    pub fn namespaces(&self) -> Result<BTreeMap<NodeId, BTreeMap<String, Oid>>> {
        Self::namespaces_in(&self.git)
    }

    /// One namespace, with the namespace prefix stripped from ref names.
    pub fn namespace(&self, node: &NodeId) -> Result<BTreeMap<String, Oid>> {
        let prefix = namespace_prefix(node);
        Ok(Self::refs_in(&self.git, &prefix)?
            .into_iter()
            .map(|(name, oid)| (name[prefix.len()..].to_string(), oid))
            .collect())
    }

    fn write_blob(&self, bytes: &[u8]) -> Result<Oid> {
        let out = self.git.run_bytes(["hash-object", "-w", "--stdin"], Some(bytes))?;
        Ok(Oid::parse(String::from_utf8_lossy(&out).trim())?)
    }

    fn read_blob(git: &Git, rev: &str, max: u64) -> Result<Vec<u8>> {
        let size: u64 = git.run(["cat-file", "-s", rev])?.parse()?;
        if size > max {
            bail!("{rev} is {size} bytes, larger than the {max} byte limit");
        }
        git.run_bytes(["cat-file", "blob", rev], None)
    }

    fn object_type(&self, oid: &Oid) -> Result<String> {
        self.git.run(["cat-file", "-t", oid.as_str()])
    }

    fn update_refs(&self, ops: &[RefOp]) -> Result<()> {
        if ops.is_empty() {
            return Ok(());
        }
        let mut input = String::new();
        for op in ops {
            let (name, line) = match op {
                RefOp::Update { name, new, old: Some(old) } => (name, format!("update {name} {new} {old}\n")),
                RefOp::Update { name, new, old: None } => (name, format!("update {name} {new}\n")),
                RefOp::Delete { name, old: Some(old) } => (name, format!("delete {name} {old}\n")),
                RefOp::Delete { name, old: None } => (name, format!("delete {name}\n")),
            };
            validate_ref_name(name)?;
            input.push_str(&line);
        }
        // `update-ref --stdin` applies every line in one transaction.
        self.git.run_bytes(["update-ref", "--stdin"], Some(input.as_bytes()))?;
        Ok(())
    }

    // ---- signed refs -----------------------------------------------------

    pub fn sigrefs(&self, node: &NodeId) -> Result<Option<(Oid, SignedRefs)>> {
        let Some(oid) = self.namespace(node)?.remove(SIGREFS_REF) else { return Ok(None) };
        let bytes = Self::read_blob(&self.git, oid.as_str(), MAX_MANIFEST_BYTES as u64)?;
        Ok(Some((oid, SignedRefs::parse(&bytes)?)))
    }

    /// Re-signs our own namespace if its refs changed. Returns the current
    /// sequence number and whether a new manifest was written.
    pub fn sign_refs(&self, secret: &SecretKey) -> Result<(u64, bool)> {
        let me = NodeId::of(secret);
        let mut refs = self.namespace(&me)?;
        refs.remove(SIGREFS_REF);
        let previous = self.sigrefs(&me)?;
        if let Some((_, prev)) = &previous
            && prev.refs == refs
        {
            return Ok((prev.seq, false));
        }
        let manifest = SignedRefs::sign(self.repo, previous.as_ref().map(|(_, p)| p), refs, secret);
        let blob = self.write_blob(manifest.raw())?;
        self.update_refs(&[RefOp::Update {
            name: format!("{}{SIGREFS_REF}", namespace_prefix(&me)),
            new: blob,
            old: previous.map(|(oid, _)| oid),
        }])?;
        Ok((manifest.seq, true))
    }

    // ---- identity --------------------------------------------------------

    /// Appends `revision` to the identity history in our namespace.
    pub fn write_identity(&self, secret: &SecretKey, revision: &Revision) -> Result<Oid> {
        let me = NodeId::of(secret);
        let id_ref = format!("{}{IDENTITY_REF}", namespace_prefix(&me));
        let doc = self.write_blob(&revision.doc.canonical_bytes())?;
        let sigs = self.write_blob(&revision.signatures_bytes())?;
        let listing = format!("100644 blob {doc}\tdoc.json\n100644 blob {sigs}\tsignatures.json\n");
        let tree = self.git.run_bytes(["mktree"], Some(listing.as_bytes()))?;
        let tree = String::from_utf8_lossy(&tree).trim().to_string();

        let parent = self.namespace(&me)?.remove(IDENTITY_REF);
        // Identity revisions carry their own Ed25519 signatures; a user's
        // commit.gpgSign setting must not trigger a GPG prompt here.
        let mut args = vec!["commit-tree".to_string(), "--no-gpg-sign".into(), tree, "-m".into(), "og identity revision".into()];
        if let Some(p) = &parent {
            args.extend(["-p".into(), p.to_string()]);
        }
        let email = format!("{me}@og");
        let git = self.git.clone().with_env("GIT_AUTHOR_NAME", "og").with_env("GIT_AUTHOR_EMAIL", &email);
        let git = git.with_env("GIT_COMMITTER_NAME", "og").with_env("GIT_COMMITTER_EMAIL", &email);
        let commit = Oid::parse(&git.run(&args)?)?;
        self.update_refs(&[RefOp::Update { name: id_ref, new: commit.clone(), old: parent }])?;
        Ok(commit)
    }

    /// Reads the identity history ending at `tip`, oldest revision first.
    pub fn identity_history(&self, tip: &Oid) -> Result<Vec<Revision>> {
        let limit = format!("--max-count={}", MAX_IDENTITY_REVISIONS + 1);
        let commits = self.git.run(["rev-list", "--first-parent", &limit, tip.as_str()])?;
        let mut commits: Vec<&str> = commits.lines().collect();
        if commits.len() > MAX_IDENTITY_REVISIONS {
            bail!("identity history is longer than {MAX_IDENTITY_REVISIONS} revisions");
        }
        commits.reverse();
        commits
            .into_iter()
            .map(|c| {
                let doc = Self::read_blob(&self.git, &format!("{c}:doc.json"), MAX_IDENTITY_FILE)?;
                let sigs = Self::read_blob(&self.git, &format!("{c}:signatures.json"), MAX_IDENTITY_FILE)?;
                Ok(Revision::from_parts(&doc, &sigs)?)
            })
            .collect()
    }

    /// The longest valid identity history found in any signed namespace.
    pub fn identity(&self) -> Result<Identity> {
        let mut best: Option<Identity> = None;
        for (node, refs) in self.namespaces()? {
            let (Some(tip), true) = (refs.get(IDENTITY_REF), refs.contains_key(SIGREFS_REF)) else {
                continue;
            };
            let verified = self
                .identity_history(tip)
                .and_then(|h| Ok((verify_history(&self.repo, &h)?.clone(), h.len())));
            match verified {
                Ok((doc, revisions)) => {
                    let better = best.as_ref().is_none_or(|b| {
                        revisions > b.revisions
                            || (revisions == b.revisions && doc.hash().as_bytes() < b.doc.hash().as_bytes())
                    });
                    if better {
                        best = Some(Identity { doc, revisions, source: node });
                    }
                }
                Err(e) => tracing::debug!(%node, "ignoring identity: {e:#}"),
            }
        }
        best.ok_or_else(|| anyhow!("no valid identity document for {} yet", self.repo))
    }

    // ---- canonical refs --------------------------------------------------

    pub fn canonical(&self) -> Result<CanonicalView> {
        let identity = self.identity()?;
        let namespaces = self.namespaces()?;
        let mut heads: BTreeMap<String, BTreeMap<NodeId, Oid>> = BTreeMap::new();
        let mut tags: BTreeMap<String, BTreeMap<NodeId, Oid>> = BTreeMap::new();
        for delegate in &identity.doc.delegates {
            let Some(refs) = namespaces.get(delegate).filter(|r| r.contains_key(SIGREFS_REF)) else {
                continue;
            };
            for (name, oid) in refs {
                let group = if name.starts_with("refs/heads/") {
                    &mut heads
                } else if name.starts_with("refs/tags/") {
                    &mut tags
                } else {
                    continue;
                };
                group.entry(name.clone()).or_default().insert(*delegate, oid.clone());
            }
        }

        let threshold = identity.doc.threshold;
        let mut branches = Vec::new();
        for (name, tips) in heads {
            match branch_quorum(self, &tips, threshold) {
                Ok(quorum) => branches.push(RefQuorum { name, tips, quorum }),
                Err(e) => tracing::warn!(branch = %name, "cannot compute quorum: {e:#}"),
            }
        }
        let tags = tags
            .into_iter()
            .map(|(name, tips)| {
                let quorum = exact_quorum(&tips, threshold);
                RefQuorum { name, tips, quorum }
            })
            .collect();
        Ok(CanonicalView { identity, branches, tags })
    }

    /// The refs `git fetch` and `git clone` see.
    pub fn advertised(&self) -> Result<Advertised> {
        let view = self.canonical()?;
        let mut refs: Vec<(Oid, String)> = view
            .branches
            .iter()
            .chain(&view.tags)
            .filter_map(|r| r.quorum.oid().map(|oid| (oid.clone(), r.name.clone())))
            .collect();
        let default = format!("refs/heads/{}", view.identity.doc.default_branch);
        let head = refs.iter().any(|(_, name)| *name == default).then_some(default);

        for (node, ns) in self.namespaces()? {
            if !ns.contains_key(SIGREFS_REF) {
                continue;
            }
            for (name, oid) in ns {
                if let Some(rest) = name.strip_prefix("refs/").filter(|r| r.starts_with("heads/") || r.starts_with("tags/")) {
                    refs.push((oid, format!("refs/peers/{node}/{rest}")));
                }
            }
        }
        Ok(Advertised { refs, head })
    }

    /// Our own branches and tags, which is what `git push` compares against.
    pub fn own_refs(&self, me: &NodeId) -> Result<Vec<(Oid, String)>> {
        Ok(self
            .namespace(me)?
            .into_iter()
            .filter(|(name, _)| name.starts_with("refs/heads/") || name.starts_with("refs/tags/"))
            .map(|(name, oid)| (oid, name))
            .collect())
    }

    // ---- push ------------------------------------------------------------

    /// Copies objects from the user's repo at `source` and applies `updates`
    /// to our own namespace, then re-signs it. Like `git push`, each ref
    /// succeeds or fails on its own.
    pub fn push(&self, secret: &SecretKey, source: &Path, updates: &[PushUpdate]) -> Result<PushOutcome> {
        let me = NodeId::of(secret);
        let prefix = namespace_prefix(&me);

        let wants: BTreeSet<&Oid> = updates.iter().filter_map(|u| u.src.as_ref()).collect();
        if !wants.is_empty() {
            // Protocol v2 lets a local fetch ask for any object, so no refs are needed.
            let mut args: Vec<String> = [
                "-c", "protocol.version=2", "-c", "gc.auto=0", "-c", "maintenance.auto=false",
                "fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "--no-recurse-submodules",
            ]
            .map(String::from)
            .into();
            args.push(source.to_string_lossy().into_owned());
            args.extend(wants.iter().map(|o| o.to_string()));
            self.git.run(&args).context("copying objects from the working repo")?;
        }

        let current = self.namespace(&me)?;
        let mut ops = Vec::new();
        let mut results = Vec::new();
        for update in updates {
            match self.check_push(update, &prefix, current.get(&update.dst)) {
                Ok(op) => {
                    ops.extend(op);
                    results.push((update.dst.clone(), None));
                }
                Err(e) => results.push((update.dst.clone(), Some(format!("{e:#}")))),
            }
        }
        if let Err(e) = self.update_refs(&ops) {
            for (_, error) in results.iter_mut().filter(|(_, e)| e.is_none()) {
                *error = Some(format!("{e:#}"));
            }
        }
        let (seq, changed) = self.sign_refs(secret)?;
        Ok(PushOutcome { results, seq: changed.then_some(seq) })
    }

    fn check_push(&self, update: &PushUpdate, prefix: &str, old: Option<&Oid>) -> Result<Option<RefOp>> {
        let is_branch = update.dst.starts_with("refs/heads/");
        if !is_branch && !update.dst.starts_with("refs/tags/") {
            bail!("only branches and tags can be pushed");
        }
        validate_ref_name(&update.dst)?;
        let name = format!("{prefix}{}", update.dst);
        let old = old.cloned();
        match (&update.src, old) {
            (None, None) => bail!("no such ref"),
            (None, Some(old)) => Ok(Some(RefOp::Delete { name, old: Some(old) })),
            (Some(new), Some(old)) if *new == old => Ok(None),
            (Some(new), old) => {
                if is_branch {
                    if self.object_type(new)? != "commit" {
                        bail!("a branch must point to a commit");
                    }
                    if let Some(old) = &old
                        && !update.force
                        && !self.is_ancestor(old, new)?
                    {
                        bail!("non-fast-forward");
                    }
                } else if old.is_some() && !update.force {
                    bail!("already exists");
                }
                Ok(Some(RefOp::Update { name, new: new.clone(), old }))
            }
        }
    }

    // ---- replication -----------------------------------------------------

    /// Fetches from `url` into a fresh quarantine repo that borrows the
    /// store's objects, so Git's negotiation only transfers what is missing.
    /// `namespace` limits the fetch to one peer (the server must serve it
    /// with `GIT_NAMESPACE`); `None` fetches every namespace.
    pub fn fetch_quarantined(&self, url: &str, namespace: Option<&NodeId>) -> Result<Quarantine> {
        fs::create_dir_all(&self.tmp)?;
        let dir = tempfile::Builder::new().prefix("quarantine-").tempdir_in(&self.tmp)?;
        let status = clean_env(&mut Command::new("git"))
            .args(["init", "--bare", "--quiet"])
            .arg(dir.path())
            .status()?;
        if !status.success() {
            bail!("git init failed for quarantine");
        }
        let objects = fs::canonicalize(self.git.dir().join("objects"))?;
        fs::write(dir.path().join("objects/info/alternates"), format!("{}\n", objects.display()))?;

        let refspec = match namespace {
            None => "+refs/namespaces/*:refs/namespaces/*".to_string(),
            Some(node) => format!("+refs/*:refs/namespaces/{node}/refs/*"),
        };
        let git = Git::new(dir.path());
        git.run([
            "-c", "protocol.version=0",
            "-c", "protocol.ext.allow=always",
            "-c", "fetch.fsckObjects=true",
            "-c", "gc.auto=0",
            "-c", "maintenance.auto=false",
            "fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "--no-recurse-submodules",
            url, &refspec,
        ])
        .context("fetching from peer")?;
        Ok(Quarantine { dir, git })
    }

    /// Verifies every namespace in `quarantine` and moves the ones that pass
    /// into the store. Call with the repo lock held.
    pub fn apply(&self, quarantine: Quarantine) -> Result<SyncReport> {
        let fetched = Self::namespaces_in(&quarantine.git)?;
        let objects = quarantine.dir.path().join("objects");
        // Sees the store's objects plus the quarantined ones.
        let checker = self.git.clone().with_env("GIT_ALTERNATE_OBJECT_DIRECTORIES", objects.to_string_lossy());

        let mut report = SyncReport::default();
        let mut ops = Vec::new();
        for (node, refs) in fetched {
            match self.verify_namespace(&quarantine, &checker, &node, &refs) {
                Ok(Verdict::Same) => {}
                Ok(Verdict::Stale) => report.stale.push(node),
                Ok(Verdict::Apply(sig_oid, manifest)) => {
                    let prefix = namespace_prefix(&node);
                    let current = self.namespace(&node)?;
                    for (name, oid) in &manifest.refs {
                        if current.get(name) != Some(oid) {
                            ops.push(RefOp::Update { name: format!("{prefix}{name}"), new: oid.clone(), old: None });
                        }
                    }
                    for name in current.keys() {
                        if name != SIGREFS_REF && !manifest.refs.contains_key(name) {
                            ops.push(RefOp::Delete { name: format!("{prefix}{name}"), old: None });
                        }
                    }
                    ops.push(RefOp::Update { name: format!("{prefix}{SIGREFS_REF}"), new: sig_oid, old: None });
                    report.updated.push((node, manifest.seq));
                }
                Err(e) => report.rejected.push(format!("{node}: {e:#}")),
            }
        }
        if !ops.is_empty() {
            migrate_objects(&objects, &self.git.dir().join("objects"))?;
            self.update_refs(&ops)?;
        }
        Ok(report)
    }

    /// Decides whether to apply a fetched namespace. An error means it must
    /// be rejected.
    fn verify_namespace(
        &self,
        quarantine: &Quarantine,
        checker: &Git,
        node: &NodeId,
        refs: &BTreeMap<String, Oid>,
    ) -> Result<Verdict> {
        let sig_oid = refs.get(SIGREFS_REF).ok_or_else(|| anyhow!("namespace has no signed refs"))?;
        let bytes = Self::read_blob(&quarantine.git, sig_oid.as_str(), MAX_MANIFEST_BYTES as u64)?;
        let manifest = SignedRefs::parse(&bytes)?;
        manifest.verify(node, &self.repo)?;

        let stored = self.sigrefs(node)?.map(|(_, s)| s);
        match check_update(stored.as_ref(), &manifest) {
            Update::Newer => {}
            Update::Same => return Ok(Verdict::Same),
            Update::Rollback { .. } => return Ok(Verdict::Stale),
            Update::Equivocation { seq } => bail!("owner signed two different manifests with seq {seq}"),
        }

        // Every listed object, and everything it references, must be present.
        if !manifest.refs.is_empty() {
            let mut input: String = manifest.refs.values().map(|o| format!("{o}\n")).collect();
            input.push_str("--not\n--all\n");
            checker
                .run_bytes(["rev-list", "--objects", "--quiet", "--stdin"], Some(input.as_bytes()))
                .context("manifest lists objects the peer did not send")?;
        }
        Ok(Verdict::Apply(sig_oid.clone(), Box::new(manifest)))
    }
}

impl History for Store {
    type Error = anyhow::Error;

    fn is_ancestor(&self, ancestor: &Oid, descendant: &Oid) -> Result<bool> {
        self.git.test(["merge-base", "--is-ancestor", ancestor.as_str(), descendant.as_str()])
    }

    fn merge_bases(&self, oids: &[Oid]) -> Result<Vec<Oid>> {
        let mut args = vec!["merge-base", "--all", "--octopus"];
        args.extend(oids.iter().map(Oid::as_str));
        let out = self.git.output(&args, None)?;
        match out.status.code() {
            Some(0) => String::from_utf8_lossy(&out.stdout).lines().map(|l| Ok(Oid::parse(l)?)).collect(),
            Some(1) => Ok(Vec::new()),
            _ => bail!("git merge-base failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
        }
    }
}

/// Moves verified objects from a quarantine into the store. Pack data goes
/// before its index, because Git discovers packs through their `.idx`.
fn migrate_objects(from: &Path, to: &Path) -> Result<()> {
    let move_file = |src: &Path, dst: &Path| -> Result<()> {
        if dst.exists() {
            return Ok(());
        }
        fs::create_dir_all(dst.parent().expect("object path has a parent"))?;
        fs::rename(src, dst).with_context(|| format!("moving {}", src.display()))
    };

    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
            for object in fs::read_dir(entry.path())? {
                let object = object?;
                move_file(&object.path(), &to.join(&name).join(object.file_name()))?;
            }
        }
    }

    let pack_dir = from.join("pack");
    if pack_dir.is_dir() {
        let mut files: Vec<PathBuf> = fs::read_dir(&pack_dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                let name = p.file_name().unwrap_or_default().to_string_lossy();
                name.starts_with("pack-") && !name.ends_with(".keep")
            })
            .collect();
        files.sort_by_key(|p| p.extension().is_some_and(|e| e == "idx"));
        for file in files {
            move_file(&file, &to.join("pack").join(file.file_name().expect("pack file has a name")))?;
        }
    }
    Ok(())
}
