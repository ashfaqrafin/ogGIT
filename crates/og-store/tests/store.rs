//! Store behaviour against real `git`: pushes, peer sync through a
//! quarantine, and the rejection rules. Peers are reached by local path here;
//! the daemon swaps in an Iroh stream for the same fetch.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use og_core::{Doc, NodeId, Oid, PushUpdate, SecretKey, namespace_prefix};
use og_store::{Store, clean_env};

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        Self { _tmp: tmp, root }
    }

    fn dir(&self, name: &str) -> PathBuf {
        let p = self.root.join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = clean_env(&mut Command::new("git"))
        .current_dir(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn commit(work: &Path, file: &str) -> Oid {
    std::fs::write(work.join(file), file).unwrap();
    git(work, &["add", file]);
    git(work, &["commit", "-q", "-m", file]);
    Oid::parse(&git(work, &["rev-parse", "HEAD"])).unwrap()
}

fn push(store: &Store, key: &SecretKey, work: &Path, oid: &Oid, force: bool) -> Option<String> {
    let update = PushUpdate { src: Some(oid.clone()), dst: "refs/heads/main".into(), force };
    let outcome = store.push(key, &work.join(".git"), &[update]).unwrap();
    outcome.results[0].1.clone()
}

fn sync(to: &Store, from: &Store) -> og_store::SyncReport {
    let q = to.fetch_quarantined(&from.git_dir().to_string_lossy(), None).unwrap();
    to.apply(q).unwrap()
}

#[test]
fn push_sync_and_reject() {
    let fx = Fixture::new();
    let alice = SecretKey::generate();
    let alice_id = NodeId::of(&alice);
    let doc = Doc::new("demo", "main", vec![alice_id], 1);
    let a = Store::init(&fx.dir("alice"), &alice, doc).unwrap();
    let repo = a.repo();

    let work = fx.dir("work");
    git(&work, &["init", "-q", "-b", "main"]);
    let c1 = commit(&work, "one");
    assert_eq!(push(&a, &alice, &work, &c1, false), None);

    let adv = a.advertised().unwrap();
    assert!(adv.refs.contains(&(c1.clone(), "refs/heads/main".into())));
    assert!(adv.refs.contains(&(c1.clone(), format!("refs/peers/{alice_id}/heads/main"))));
    assert_eq!(adv.head.as_deref(), Some("refs/heads/main"));

    // Bob replicates Alice's namespace.
    let b = Store::open_or_create(&fx.dir("bob"), repo).unwrap();
    let report = sync(&b, &a);
    assert_eq!(report.updated, vec![(alice_id, 2)]);
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);
    assert_eq!(b.canonical().unwrap().branches[0].quorum.oid(), Some(&c1));
    assert!(sync(&b, &a).updated.is_empty(), "second sync is a no-op");

    // Non-fast-forward pushes need force.
    let c2 = commit(&work, "two");
    git(&work, &["reset", "-q", "--hard", c1.as_str()]);
    let c2b = commit(&work, "two-b");
    assert_eq!(push(&a, &alice, &work, &c2, false), None);
    assert_eq!(push(&a, &alice, &work, &c2b, false).as_deref(), Some("non-fast-forward"));
    assert_eq!(push(&a, &alice, &work, &c2b, true), None);

    // Keep a copy of Alice's store at this point, then move on.
    let stale = fx.root.join("stale.git");
    let status = Command::new("cp").arg("-a").arg(a.git_dir()).arg(&stale).status().unwrap();
    assert!(status.success());
    let c3 = commit(&work, "three");
    assert_eq!(push(&a, &alice, &work, &c3, false), None);
    assert_eq!(sync(&b, &a).updated, vec![(alice_id, 5)]);

    // Serving Bob the old manifest is a rollback: ignored as stale.
    let q = b.fetch_quarantined(&stale.to_string_lossy(), None).unwrap();
    let report = b.apply(q).unwrap();
    assert!(report.updated.is_empty());
    assert_eq!(report.stale, vec![alice_id]);
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);
    assert_eq!(b.canonical().unwrap().branches[0].quorum.oid(), Some(&c3));
}

#[test]
fn forged_namespace_is_rejected() {
    let fx = Fixture::new();
    let alice = SecretKey::generate();
    let alice_id = NodeId::of(&alice);
    let a = Store::init(&fx.dir("alice"), &alice, Doc::new("demo", "main", vec![alice_id], 1)).unwrap();

    // Mallory copies Alice's repo, then plants her own signed refs under
    // Alice's namespace with a higher sequence number.
    let mallory = SecretKey::generate();
    let m = Store::open_or_create(&fx.dir("mallory"), a.repo()).unwrap();
    sync(&m, &a);
    let work = fx.dir("work");
    git(&work, &["init", "-q", "-b", "main"]);
    let evil = commit(&work, "evil");
    for _ in 0..3 {
        push(&m, &mallory, &work, &evil, true);
        git(&work, &["commit", "-q", "--allow-empty", "-m", "bump"]);
    }
    let mallory_ns = namespace_prefix(&NodeId::of(&mallory));
    let alice_ns = namespace_prefix(&alice_id);
    for (name, oid) in m.namespace(&NodeId::of(&mallory)).unwrap() {
        git(m.git_dir(), &["update-ref", &format!("{alice_ns}{name}"), oid.as_str()]);
        git(m.git_dir(), &["update-ref", "-d", &format!("{mallory_ns}{name}")]);
    }

    let b = Store::open_or_create(&fx.dir("bob"), a.repo()).unwrap();
    sync(&b, &a);
    let report = sync(&b, &m);
    assert!(report.updated.is_empty());
    assert!(report.rejected[0].contains("signature"), "{:?}", report.rejected);
    assert!(!b.namespace(&alice_id).unwrap().contains_key("refs/heads/main"));
}

#[test]
fn quorum_across_delegates() {
    let fx = Fixture::new();
    let keys: Vec<SecretKey> = (0..3).map(|_| SecretKey::generate()).collect();
    let ids: Vec<NodeId> = keys.iter().map(NodeId::of).collect();
    let a = Store::init(&fx.dir("a"), &keys[0], Doc::new("demo", "main", ids.clone(), 2)).unwrap();

    let work = fx.dir("work");
    git(&work, &["init", "-q", "-b", "main"]);
    let c1 = commit(&work, "one");
    let c2 = commit(&work, "two");

    // Delegate 0 is at c2; nobody agrees yet.
    push(&a, &keys[0], &work, &c2, false);
    let main = |s: &Store| s.canonical().unwrap().branches[0].quorum.clone();
    assert!(main(&a).oid().is_none());

    // Delegate 1 (sharing this store for the test) reaches c1: c1 is now in
    // two histories.
    push(&a, &keys[1], &work, &c1, false);
    assert_eq!(main(&a).oid(), Some(&c1));
    push(&a, &keys[1], &work, &c2, false);
    assert_eq!(main(&a).oid(), Some(&c2));
}
