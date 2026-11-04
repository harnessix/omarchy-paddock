use super::*;
use crate::env::HostEnv;
use crate::manifest::hex;
use crate::recover::Session;
use crate::state::{self, AuthMode, Running};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The snapshots and the undo trash folders.
pub fn scan_snapshots(env: &HostEnv) -> Vec<SnapshotDir> {
    let mut out = Vec::new();
    for base in subdirs(&crate::snapshot::root(env)) {
        for (path, m) in crate::snapshot::list(&base) {
            out.push(SnapshotDir {
                path,
                project_exists: m.project.is_dir(),
                project: Some(m.project),
                harness: m.harness,
                started: m.started,
                bytes: m.bytes,
                trash: false,
            });
        }
    }
    for path in subdirs(&crate::snapshot::trash_root(env)) {
        let started = std::fs::metadata(&path)
            .map(|m| m.mtime().max(0) as u64)
            .unwrap_or(0);
        out.push(SnapshotDir {
            bytes: allocated(&path),
            path,
            project: None,
            harness: String::new(),
            started,
            project_exists: true,
            trash: true,
        });
    }
    out
}

/// The docker and podman commands, behind a trait so tests can fake them.
pub trait Containers {
    /// Run `cli args...` and return stdout. Err when the command fails.
    fn run(&self, cli: &str, args: &[&str]) -> Result<String>;
    /// The installed CLIs among docker and podman.
    fn clis(&self) -> Vec<String>;
}

pub struct RealContainers<'a>(pub &'a HostEnv);

impl Containers for RealContainers<'_> {
    fn run(&self, cli: &str, args: &[&str]) -> Result<String> {
        let exe = self
            .0
            .which(cli)
            .with_context(|| format!("{cli} not found"))?;
        crate::env::capture(Command::new(exe).args(args).stdin(Stdio::null()))
    }
    fn clis(&self) -> Vec<String> {
        ["docker", "podman"]
            .into_iter()
            .filter(|c| self.0.which(c).is_some())
            .map(str::to_string)
            .collect()
    }
}

/// Facts from outside the data folders: containers, current image tags and
/// the known profiles.
pub struct Host<'a> {
    pub containers: &'a dyn Containers,
    pub current_tags: BTreeSet<String>,
    /// Profile name to "is a desktop app". None when the profiles cannot be
    /// read: then no home counts as orphaned.
    pub profiles: Option<BTreeMap<String, bool>>,
    /// Profile name to its pure cache paths. A home of another harness
    /// uses the table alone.
    pub scratch: BTreeMap<String, Vec<String>>,
}

impl<'a> Host<'a> {
    pub fn real(env: &HostEnv, containers: &'a dyn Containers) -> Self {
        let mut scratch = BTreeMap::new();
        let profiles = crate::profile::HarnessFactory::new(env).ok().map(|f| {
            f.names()
                .into_iter()
                .map(|n| {
                    let p = f.profile(&n);
                    scratch.insert(n.clone(), caches::scratch(p).0);
                    let app = p.is_some_and(|p| p.app.is_some());
                    (n, app)
                })
                .collect()
        });
        Self {
            containers,
            current_tags: current_tags(env),
            profiles,
            scratch,
        }
    }
}

fn mtime(p: &Path) -> u64 {
    std::fs::symlink_metadata(p)
        .map(|m| m.mtime().max(0) as u64)
        .unwrap_or(0)
}

fn newest_change(home: &Path) -> u64 {
    std::fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| mtime(&e.path()))
        .fold(mtime(home), u64::max)
}

fn keyring_key(home: &Path) -> String {
    hex(&Sha256::digest(home.to_string_lossy().as_bytes()))[..24].to_string()
}

fn loses(app: bool, harness: &str, auth: &str) -> String {
    if app {
        return "the app's login, settings and local data in this home".into();
    }
    let login = match auth.strip_prefix("isolated:") {
        Some(n) => format!("the login stays in ~/.local/share/paddock/auth/{harness}/{n}"),
        None => "the login stays in your host home".into(),
    };
    format!("transcripts, history and settings of this project; {login}")
}

/// A query about refs and objects. The repositories and worktrees here are
/// writable by sandboxes, so the call never reads their configuration and
/// never touches a work tree (see hostgit.rs).
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    crate::hostgit::Layout::discover(dir)
        .ok()
        .flatten()?
        .try_query(args)
}

struct ListedWorktree {
    path: PathBuf,
    head: Option<String>,
    branch: Option<String>,
}

fn list_worktrees(repo: &Path) -> Vec<ListedWorktree> {
    let Some(out) = git(repo, &["worktree", "list", "--porcelain"]) else {
        return vec![];
    };
    let mut v = Vec::new();
    for block in out.split("\n\n") {
        let mut w = ListedWorktree {
            path: PathBuf::new(),
            head: None,
            branch: None,
        };
        for l in block.lines() {
            if let Some(p) = l.strip_prefix("worktree ") {
                w.path = PathBuf::from(p);
            } else if let Some(h) = l.strip_prefix("HEAD ") {
                w.head = Some(h.to_string());
            } else if let Some(b) = l.strip_prefix("branch refs/heads/") {
                w.branch = Some(b.to_string());
            }
        }
        if !w.path.as_os_str().is_empty() {
            v.push(w);
        }
    }
    v
}

/// Facts about one worktree. The git directory of a linked worktree is not
/// taken from its `.git` file: `Layout::discover` checks that the repository
/// registered this worktree (see hostgit.rs), so there is nothing to look up
/// by hand here.
fn worktree_facts(
    env: &HostEnv,
    repo: &Path,
    listed: &ListedWorktree,
    branch: Option<&str>,
) -> (Option<usize>, Option<bool>) {
    // The commits of the branch, else of the worktree's HEAD.
    let head = match branch {
        Some(b) => Some(format!("refs/heads/{b}")),
        None => listed.head.clone(),
    };
    let unmerged = head.as_ref().and_then(|head| {
        let ex = branch.map(|b| format!("--exclude={b}"));
        let mut args = vec!["rev-list", "--count", head.as_str(), "--not"];
        if let Some(ex) = &ex {
            args.push(ex);
        }
        args.extend(["--branches", "--remotes"]);
        git(repo, &args).and_then(|s| s.trim().parse().ok())
    });
    // The work tree holds what a sandbox wrote, so `git status` runs in a
    // sandbox of its own. The layout check also confirms that this worktree
    // is one the repository registered. None means "not known".
    let common = crate::hostgit::Layout::discover(repo)
        .ok()
        .flatten()
        .map(|l| l.common_dir);
    let dirty = crate::hostgit::Layout::discover(&listed.path)
        .ok()
        .flatten()
        .filter(|l| Some(&l.common_dir) == common.as_ref())
        .and_then(|l| l.status(env, "normal").ok().flatten())
        .map(|lines| lines.iter().any(|l| !l.trim().is_empty()));
    (unmerged, dirty)
}

/// The worktrees that paddock made (`--worktree` runs and apps), without
/// sizes. For `paddock worktree list` and the panel.
pub fn worktrees(env: &HostEnv) -> Vec<Worktree> {
    let running = Running::list(env);
    let running_projects: BTreeSet<&Path> = running.iter().map(|r| r.project.as_path()).collect();
    let candidates: Vec<PathBuf> = state::recent(env)
        .into_iter()
        .map(|r| r.project)
        .chain(Session::all(env).into_iter().map(|s| s.project))
        .chain(running.iter().filter_map(|r| r.kv_project.clone()))
        .collect();
    scan_worktrees(env, candidates, &running_projects)
}

/// The worktrees below paddock's folder. The repository of each comes from
/// host-side records and verified origins (`candidates`), matched by the
/// slug of the worktree folder.
fn scan_worktrees(
    env: &HostEnv,
    candidates: Vec<PathBuf>,
    running_projects: &BTreeSet<&Path>,
) -> Vec<Worktree> {
    let mut repos: BTreeMap<String, PathBuf> = BTreeMap::new();
    for p in candidates {
        if p.is_dir() {
            repos.entry(state::slug(&p)).or_insert(p);
        }
    }
    let mut listed: BTreeMap<PathBuf, Vec<ListedWorktree>> = BTreeMap::new();
    let mut worktrees = Vec::new();
    for sdir in subdirs(&worktrees_root(env)) {
        let repo = repos.get(&name(&sdir)).cloned();
        for path in subdirs(&sdir) {
            let mut w = Worktree {
                running: crate::paths::any_same(running_projects.iter().copied(), &path),
                path: path.clone(),
                repo: None,
                branch: None,
                unmerged: None,
                dirty: None,
                bytes: 0,
            };
            if let Some(repo) = &repo {
                let list = listed
                    .entry(repo.clone())
                    .or_insert_with(|| list_worktrees(repo));
                if let Some(l) = crate::paths::find_same(&*list, &path, |l| &l.path) {
                    // The branch that paddock made, not the one the worktree
                    // is on now. Without a record, only a `paddock/` branch.
                    let branch = match recorded_worktree_branch(env, &path) {
                        Some(b) => git(
                            repo,
                            &["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")],
                        )
                        .map(|_| b),
                        None => l.branch.clone().filter(|b| b.starts_with("paddock/")),
                    };
                    let (unmerged, dirty) = worktree_facts(env, repo, l, branch.as_deref());
                    w.repo = Some(repo.clone());
                    w.branch = branch;
                    w.unmerged = unmerged;
                    w.dirty = dirty;
                }
            }
            worktrees.push(w);
        }
    }
    worktrees
}

/// Look at every piece of paddock data. With `with_sizes`, measure homes and
/// worktrees too, which can take some seconds.
pub fn scan(env: &HostEnv, host: &Host, with_sizes: bool) -> Inventory {
    let now = state::now();
    let mut notes = Vec::new();
    let running: Vec<Running> = Running::list(env);
    let running_ids: BTreeSet<&str> = running.iter().map(|r| r.id.as_str()).collect();
    let sessions = Session::all(env);

    let mut running_homes: BTreeSet<PathBuf> = sessions
        .iter()
        .filter(|s| running_ids.contains(s.id.as_str()))
        .map(|s| s.home.clone())
        .collect();
    for r in &running {
        if let Ok(auth) = r.auth.parse::<AuthMode>() {
            running_homes.insert(
                homes_root(env)
                    .join(&r.harness)
                    .join(login_dir(&auth))
                    .join(state::slug(&r.project)),
            );
        }
    }
    let running_projects: BTreeSet<&Path> = running.iter().map(|r| r.project.as_path()).collect();
    let mut session_last: BTreeMap<&Path, u64> = BTreeMap::new();
    for s in &sessions {
        let t = s.ended.unwrap_or(s.started).max(s.started).min(now);
        let e = session_last.entry(s.home.as_path()).or_insert(0);
        *e = (*e).max(t);
    }

    // Homes.
    let mut homes = Vec::new();
    for hdir in subdirs(&homes_root(env)) {
        let harness = name(&hdir);
        for ldir in subdirs(&hdir) {
            let Some(auth) = auth_from_dir(&name(&ldir)) else {
                continue;
            };
            let auth = auth.to_string();
            for path in subdirs(&ldir) {
                let key = name(&path);
                let origin = read_origin(&path);
                let check = match &origin {
                    None => OriginCheck::Missing,
                    Some(o) if origin_matches(o, &harness, &auth, &key) => OriginCheck::Ok,
                    Some(_) => OriginCheck::Mismatch,
                };
                let origin = origin.filter(|_| check == OriginCheck::Ok);
                let project = origin.as_ref().map(|o| o.project.clone());
                let used = [
                    origin.as_ref().map(|o| o.last_used.min(now)),
                    session_last.get(path.as_path()).copied(),
                ]
                .into_iter()
                .flatten()
                .max();
                let (profile, app) = match &host.profiles {
                    // The escape probe's own harness has no profile file.
                    _ if harness == DOCTOR_HARNESS => (true, false),
                    Some(p) => match p.get(&harness) {
                        Some(app) => (true, *app),
                        None => (false, origin.as_ref().is_some_and(|o| o.app)),
                    },
                    None => (true, origin.as_ref().is_some_and(|o| o.app)),
                };
                let table;
                let scratch = match host.scratch.get(&harness) {
                    Some(s) => s,
                    None => {
                        table = caches::scratch(None).0;
                        &table
                    }
                };
                homes.push(Home {
                    caches: caches::find(&path, scratch),
                    project_exists: project.as_ref().map(|p| p.is_dir()),
                    project,
                    origin: check,
                    last_used: used,
                    modified: newest_change(&path),
                    bytes: 0,
                    loses: loses(app, &harness, &auth),
                    app,
                    profile,
                    running: running_homes.contains(&path),
                    harness: harness.clone(),
                    auth: auth.clone(),
                    path,
                });
            }
        }
    }
    if host.profiles.is_none() {
        notes.push("cannot read the harness profiles; no home counts as orphaned".into());
    }

    let candidates: Vec<PathBuf> = state::recent(env)
        .into_iter()
        .map(|r| r.project)
        .chain(sessions.iter().map(|s| s.project.clone()))
        .chain(homes.iter().filter_map(|h| h.project.clone()))
        .collect();
    let worktrees = scan_worktrees(env, candidates, &running_projects);

    // Session records.
    let session_records = sessions
        .iter()
        .map(|s| {
            let path = sessions_root(env).join(format!("{}.json", s.id));
            SessionRecord {
                bytes: file_len(&path),
                path,
                id: s.id.clone(),
                started: s.started,
                ended: s.ended,
                running: running_ids.contains(s.id.as_str()),
            }
        })
        .collect();

    // Keyring passwords: the file name is a hash of the home path.
    let mut known: BTreeSet<String> = homes.iter().map(|h| keyring_key(&h.path)).collect();
    known.extend(
        sessions
            .iter()
            .filter(|s| s.home.is_dir())
            .map(|s| keyring_key(&s.home)),
    );
    known.insert(keyring_key(&env.home));
    let mut keyrings: Vec<KeyringFile> = std::fs::read_dir(keyring_root(env))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| KeyringFile {
            bytes: file_len(&e.path()),
            home_exists: known.contains(&name(&e.path())),
            path: e.path(),
        })
        .collect();
    keyrings.sort_by(|a, b| a.path.cmp(&b.path));

    let mut images = Vec::new();
    let mut build_cache = Vec::new();
    for cli in host.containers.clis() {
        images.extend(scan_images(host, &cli, &mut notes));
        build_cache.extend(scan_build_cache(host, &cli));
    }

    let old_caches = OLD_CACHES
        .iter()
        .map(|(old, new)| (shared_cache(env).join(old), new))
        .filter(|(p, _)| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        .map(|(path, new)| OldCache {
            path,
            bytes: 0,
            replaced_by: new.to_string(),
        })
        .collect();

    let scratch = subdirs(&scratch_root(env))
        .into_iter()
        .map(|path| {
            let entries = std::fs::read_dir(&path).map(|d| d.count()).unwrap_or(0);
            let started = homes
                .iter()
                .filter(|h| h.project.as_deref() == Some(path.as_path()))
                .map(|h| h.last_active())
                .max()
                .unwrap_or(0);
            let changed = std::fs::metadata(&path)
                .map(|m| m.mtime().max(0) as u64)
                .unwrap_or(0);
            Scratch {
                harness: name(&path),
                last_active: changed.max(started),
                running: crate::paths::any_same(running_projects.iter().copied(), &path),
                entries,
                bytes: 0,
                path,
            }
        })
        .collect();
    let mut inv = Inventory {
        size_kind: SizeKind::Allocated,
        homes,
        images,
        worktrees,
        sessions: session_records,
        keyrings,
        notes,
        old_caches,
        any_running: !running.is_empty(),
        build_cache,
        scratch,
        snapshots: scan_snapshots(env),
        tmpfs_caches: caches::enabled(env),
        classes: vec![],
        layers: layers::scan(env),
    };
    if with_sizes {
        measure(&mut inv, |_| true);
        inv.classes = class_use(env, &inv.homes);
    }
    inv
}

/// Measure the cache folders of every home by class. The shared package
/// caches count once, in their shared folder.
fn class_use(env: &HostEnv, homes: &[Home]) -> Vec<ClassUse> {
    let mut paths: Vec<(caches::Class, PathBuf)> = homes
        .iter()
        .flat_map(|h| h.caches.iter().map(|(c, r)| (*c, h.path.join(r))))
        .collect();
    for (rel, c) in caches::TABLE {
        if *c != caches::Class::Shared {
            continue;
        }
        let name = match *rel {
            ".npm" => "npm",
            ".cache/pip" => "pip",
            ".cache/uv" => "uv",
            _ => continue,
        };
        let p = layers::lower(env, name);
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
            paths.push((*c, p));
        }
    }
    let list: Vec<PathBuf> = paths.iter().map(|(_, p)| p.clone()).collect();
    let (v, _) = sizes(&list);
    let mut by: BTreeMap<caches::Class, (u64, usize)> = BTreeMap::new();
    for ((c, _), b) in paths.iter().zip(v) {
        let e = by.entry(*c).or_default();
        e.0 += b;
        e.1 += 1;
    }
    [
        caches::Class::Scratch,
        caches::Class::Shared,
        caches::Class::Runtime,
    ]
    .into_iter()
    .map(|class| {
        let (bytes, folders) = by.get(&class).copied().unwrap_or_default();
        ClassUse {
            class,
            bytes,
            folders,
        }
    })
    .collect()
}

/// Fill in the sizes of the homes and worktrees that `want` names.
pub(super) fn measure(inv: &mut Inventory, want: impl Fn(&Path) -> bool) {
    let paths: Vec<PathBuf> = inv
        .homes
        .iter()
        .map(|h| &h.path)
        .chain(inv.worktrees.iter().map(|w| &w.path))
        .chain(inv.old_caches.iter().map(|c| &c.path))
        .chain(inv.layers.iter().map(|c| &c.path))
        .chain(inv.scratch.iter().map(|c| &c.path))
        .filter(|p| want(p))
        .cloned()
        .collect();
    let (v, kind) = sizes(&paths);
    inv.size_kind = kind;
    let by: BTreeMap<&PathBuf, u64> = paths.iter().zip(v).collect();
    for h in &mut inv.homes {
        if let Some(b) = by.get(&h.path) {
            h.bytes = *b;
        }
    }
    for w in &mut inv.worktrees {
        if let Some(b) = by.get(&w.path) {
            w.bytes = *b;
        }
    }
    for c in &mut inv.scratch {
        if let Some(b) = by.get(&c.path) {
            c.bytes = *b;
        }
    }
    for c in &mut inv.old_caches {
        if let Some(b) = by.get(&c.path) {
            c.bytes = *b;
        }
    }
    for c in &mut inv.layers {
        if let Some(b) = by.get(&c.path) {
            c.bytes = *b;
        }
    }
}
