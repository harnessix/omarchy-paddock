//! The loft: a private, versioned layer above each project. Agents leave
//! process files in repositories (instruction files, design and plan
//! files, Spec Kit's folders), and a push publishes them. The loft keeps
//! them in paddock's data folder instead, one loft per repository (the key
//! of pastures and of the Spec Kit store), and every sandbox of the
//! repository sees them in place.
//!
//! Layout, `<data>/loft/<repository key>/`:
//!
//! - `tree/`: the files at their project paths (`tree/CLAUDE.md`,
//!   `tree/specs/001-a/`), and the agents' notes in `tree/.paddock/`.
//! - `git/`: the git folder of the loft's own history. Only the host sees
//!   it. paddock commits `tree/` at the end of each run.
//! - `off`: `paddock loft remove` turned the loft off; files and history
//!   stay.
//!
//! `[loft]` in the user configuration, never in a project `paddock.toml`:
//! `store` (`external` or `repo`), `auto`, `paths`, `public`, `commit`.
//! `[speckit] store` stays as the short form for the Spec Kit folders.
//!
//! The notes folder `.paddock/` is in every sandbox, with or without
//! `paddock loft init`. Its `inbox/` stays the project's own: the host
//! writes request results there (broker::inbox).

pub mod git;
pub mod host;
pub mod push;

use crate::env::HostEnv;
use crate::spec::{Mount, SandboxSpec};
use crate::speckit::{self, Store};
use anyhow::Result;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The notes folder of agents, in the project and in `tree/`.
pub const NOTES: &str = ".paddock";

/// The line of the session note (and the paddock-host skill) about notes.
pub const NOTES_LINE: &str = "paddock: keep your own design notes, plans and reviews in .paddock/ of the project. It is private and not in git. Do not put them in docs/ or the project root.";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Commit {
    /// One commit at the end of each run.
    #[default]
    Run,
    /// Only `paddock loft commit`.
    Manual,
}

/// `[loft]` of the user configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Cfg {
    pub store: Store,
    pub auto: bool,
    pub paths: Vec<String>,
    pub public: Vec<String>,
    pub commit: Commit,
}

impl Default for Cfg {
    fn default() -> Self {
        Self {
            store: Store::External,
            auto: false,
            paths: Vec::new(),
            public: Vec::new(),
            commit: Commit::Run,
        }
    }
}

impl Cfg {
    pub fn parse(text: &str) -> Result<Self> {
        let v: toml::Value = toml::from_str(text)?;
        Ok(match v.get("loft") {
            Some(t) => t.clone().try_into()?,
            None => Self::default(),
        })
    }

    /// The user configuration. A bad table is a warning and the default.
    pub fn load(env: &HostEnv) -> Self {
        let file = env.paddock_config().join("config.toml");
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Self::default();
        };
        Self::parse(&text).unwrap_or_else(|e| {
            log::warn!("[loft] in {}: {e:#}", file.display());
            Self::default()
        })
    }
}

/// What a private path holds, for the notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Instructions,
    Design,
    SpecKit,
    User,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Instructions => "agent instruction files",
            Kind::Design => "design and plan files",
            Kind::SpecKit => "Spec Kit files",
            Kind::User => "files of your [loft] paths",
        }
    }
}

/// The built-in private paths. A folder ends with `/`.
pub const BUILT_IN: &[(&str, Kind)] = &[
    ("CLAUDE.md", Kind::Instructions),
    ("CLAUDE.local.md", Kind::Instructions),
    ("AGENTS.md", Kind::Instructions),
    ("GEMINI.md", Kind::Instructions),
    ("QWEN.md", Kind::Instructions),
    (".cursorrules", Kind::Instructions),
    (".cursor/rules/", Kind::Instructions),
    (".windsurfrules", Kind::Instructions),
    (".clinerules", Kind::Instructions),
    (".claude/settings.local.json", Kind::Instructions),
    ("DESIGN.md", Kind::Design),
    ("PLAN.md", Kind::Design),
    ("NOTES.md", Kind::Design),
    ("TODO.md", Kind::Design),
    (".specify/", Kind::SpecKit),
    ("specs/", Kind::SpecKit),
];

/// One private path, relative to the project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// A folder ends with `/`.
    pub path: String,
    pub kind: Kind,
    /// The user keeps it in git on purpose: no notice, no store.
    pub public: bool,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.path.ends_with('/')
    }

    pub fn rel(&self) -> &Path {
        Path::new(self.path.trim_end_matches('/'))
    }

    /// A plain file name such as `CLAUDE.md` also counts in subfolders.
    fn any_depth(&self) -> bool {
        !self.is_dir() && !self.path.contains('/')
    }

    /// A repository path that this entry covers.
    pub fn matches(&self, name: &str) -> bool {
        if self.is_dir() {
            name.starts_with(&self.path)
        } else {
            name == self.path || (self.any_depth() && name.ends_with(&format!("/{}", self.path)))
        }
    }

    /// The git pathspec, below `base` (a folder of the repository or "").
    pub fn pathspec(&self, base: &str) -> String {
        let base = base.trim_matches('/');
        let pre = if base.is_empty() {
            String::new()
        } else {
            format!("{base}/")
        };
        if self.any_depth() {
            format!(":(glob){pre}**/{}", self.path)
        } else if self.is_dir() {
            format!(":(glob){pre}{}**", self.path)
        } else {
            format!(":(literal){pre}{}", self.path)
        }
    }
}

/// A user path in the list form: relative, no `..`, no `.git`, folders
/// with one `/` at the end. None for a path that cannot be private.
pub fn normalize(p: &str) -> Option<String> {
    let dir = p.ends_with('/');
    let parts: Vec<&str> = p
        .trim()
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    if parts.is_empty()
        || parts
            .iter()
            .any(|c| *c == ".." || *c == ".git" || c.contains(['*', '?', '[', '\\', '\0']))
        || parts[0] == NOTES
    {
        return None;
    }
    let mut s = parts.join("/");
    if dir {
        s.push('/');
    }
    Some(s)
}

/// The list of private paths: built in, then the user's own, each marked
/// public when the user says so.
pub fn entries(cfg: &Cfg) -> Vec<Entry> {
    let public: Vec<String> = cfg
        .public
        .iter()
        .filter_map(|p| normalize(p))
        .map(|p| p.trim_end_matches('/').to_string())
        .collect();
    let mut out: Vec<Entry> = BUILT_IN
        .iter()
        .map(|(p, k)| (p.to_string(), *k))
        .chain(cfg.paths.iter().filter_map(|p| match normalize(p) {
            Some(n) => Some((n, Kind::User)),
            None => {
                log::warn!("[loft] paths: {p:?} cannot be a private path");
                None
            }
        }))
        .map(|(path, kind)| Entry {
            public: public.iter().any(|p| p == path.trim_end_matches('/')),
            path,
            kind,
        })
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|e| seen.insert(e.path.trim_end_matches('/').to_string()));
    out
}

/// The loft of one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loft {
    pub root: PathBuf,
}

impl Loft {
    /// The loft of the repository of `dir` (a worktree counts as its main
    /// work tree). The key is the physical path: `repo_of` keeps the
    /// spelling of `dir`, and on macOS `/var/folders/...` and
    /// `/private/var/folders/...` are the same repository.
    pub fn of(env: &HostEnv, dir: &Path) -> Self {
        let repo = speckit::repo_of(dir);
        let repo = std::fs::canonicalize(&repo).unwrap_or(repo);
        Self {
            root: env
                .paddock_data()
                .join("loft")
                .join(crate::kv::shard::project_hash(&repo)),
        }
    }

    pub fn tree(&self) -> PathBuf {
        self.root.join("tree")
    }

    pub fn git_dir(&self) -> PathBuf {
        self.root.join("git")
    }

    pub fn notes(&self) -> PathBuf {
        self.tree().join(NOTES)
    }

    pub fn initialized(&self) -> bool {
        self.git_dir().join("HEAD").is_file()
    }

    pub fn off(&self) -> bool {
        self.root.join("off").exists()
    }

    /// `paddock loft init` (or `auto`) made it, and it is not off.
    pub fn active(&self) -> bool {
        self.initialized() && !self.off()
    }

    /// Where an entry lives in the loft.
    pub fn path(&self, e: &Entry) -> PathBuf {
        self.tree().join(e.rel())
    }
}

/// The Spec Kit store: the loft's tree. A store of the first Spec Kit
/// release (`<data>/speckit/<key>/`) moves there once.
pub fn speckit_tree(env: &HostEnv, repo: &Path) -> PathBuf {
    let loft = Loft::of(env, repo);
    let tree = loft.tree();
    let old = env
        .paddock_data()
        .join("speckit")
        .join(crate::kv::shard::project_hash(&speckit::repo_of(repo)));
    if old.join(speckit::SPECIFY).is_dir() && !tree.join(speckit::SPECIFY).exists() {
        let moved = crate::privfs::dir(&tree).is_ok()
            && [speckit::SPECIFY, speckit::SPECS].iter().all(|n| {
                !old.join(n).exists() || std::fs::rename(old.join(n), tree.join(n)).is_ok()
            });
        if moved {
            let _ = std::fs::remove_dir(&old);
        } else {
            log::warn!(
                "cannot move the Spec Kit store {} into the loft {}",
                old.display(),
                tree.display()
            );
        }
    }
    tree
}

/// The Spec Kit folders live in the loft: `[speckit] store` when the user
/// set it, else an active loft with the external store.
pub fn speckit_external(env: &HostEnv, sk: &speckit::Cfg, project: &Path) -> bool {
    if sk.store_explicit {
        return sk.store == Store::External;
    }
    let cfg = Cfg::load(env);
    cfg.store == Store::External
        && Loft::of(env, project).active()
        && !entries(&cfg)
            .iter()
            .any(|e| e.kind == Kind::SpecKit && e.public)
}

/// The entry lives in the loft (Spec Kit entries go through speckit.rs).
pub fn external(cfg: &Cfg, active: bool, e: &Entry) -> bool {
    !e.public && e.kind != Kind::SpecKit && active && cfg.store == Store::External
}

/// Add `lines` to `info/exclude` of the repository of `project`, below
/// `header`. Lines are root paths of the project, such as `/CLAUDE.md`.
/// The file is read and replaced through safefs: a sandbox can write the
/// repository. Returns true when it changed the file.
pub fn exclude(project: &Path, header: &str, lines: &[String]) -> Result<bool> {
    let Some(l) = crate::hostgit::Layout::discover(project)? else {
        return Ok(false);
    };
    let rel = project.strip_prefix(&l.work_tree).unwrap_or(Path::new(""));
    let base = rel.display().to_string();
    let base = base.trim_matches('/');
    let root = crate::safefs::Root::open(&l.common_dir)?;
    let file = Path::new("info/exclude");
    let before = root
        .read(file, speckit::MAX_FILE)?
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let missing: Vec<String> = lines
        .iter()
        .map(|n| {
            if base.is_empty() {
                n.clone()
            } else {
                format!("/{base}{n}")
            }
        })
        .filter(|l| !before.lines().any(|b| b.trim() == l))
        .collect();
    if missing.is_empty() {
        return Ok(false);
    }
    let mut text = before;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("# paddock: {header}\n"));
    for l in missing {
        text.push_str(&l);
        text.push('\n');
    }
    root.write(file, text.as_bytes(), 0o644)?;
    Ok(true)
}

/// The exclude line of an entry or of the notes link.
pub fn exclude_line(e: &Entry) -> String {
    format!("/{}", e.path)
}

/// The loft mounts its paths over the project's paths. Those are bind
/// mounts at a new place, which bwrap and the containers give, but
/// Seatbelt and the VM backend on macOS do not.
pub const IN_PLACE: bool = cfg!(not(target_os = "macos"));

/// The build step: the notes folder, the loft's private paths, and the
/// Spec Kit store. `repo` is the repository key (the main work tree of a
/// worktree).
pub fn mount(env: &HostEnv, spec: &mut SandboxSpec, repo: &Path) -> Result<()> {
    mount_with(env, spec, repo, IN_PLACE)
}

/// [`mount`]. Without `in_place` (macOS), the sandbox gets the notes only
/// through the `.paddock` link of `paddock loft init`, and the private
/// paths stay in the project. The exclude lines keep them out of git.
pub fn mount_with(
    env: &HostEnv,
    spec: &mut SandboxSpec,
    repo: &Path,
    in_place: bool,
) -> Result<()> {
    let cfg = Cfg::load(env);
    let loft = Loft::of(env, repo);
    if cfg.auto && !loft.initialized() && !loft.off() {
        match host::init(env, repo) {
            Ok(msg) => eprintln!("paddock: {msg}"),
            Err(e) => log::warn!("cannot make the loft of {}: {e:#}", repo.display()),
        }
    }
    let project = spec.project.clone();
    let mut lines = vec![format!("/{NOTES}")];
    if let Err(e) = notes(spec, &loft, &project, in_place) {
        log::warn!("no notes folder .paddock/ in this sandbox: {e:#}");
    }
    let active = loft.active();
    let private: Vec<Entry> = entries(&cfg)
        .into_iter()
        .filter(|e| external(&cfg, active, e))
        .collect();
    if in_place {
        for e in &private {
            if mount_entry(spec, &loft, &project, e)? {
                lines.push(exclude_line(e));
            }
        }
    } else {
        if private
            .iter()
            .any(|e| std::fs::symlink_metadata(loft.path(e)).is_ok())
        {
            log::warn!(
                "the loft cannot show its files in place on macOS: the sandbox sees the project's own files, and git ignores them"
            );
        }
        lines.extend(private.iter().map(exclude_line));
    }
    if let Err(e) = exclude(&project, "the loft", &lines) {
        log::warn!("cannot keep the loft out of git: {e:#}");
    }
    if in_place {
        return speckit::mount(env, spec, repo);
    }
    let sk = speckit::Cfg::load(env);
    if sk.enabled && speckit_external(env, &sk, repo) {
        log::warn!("the Spec Kit store cannot show its folders in place on macOS: the sandbox sees the project's own .specify/ and specs/, and git ignores them");
        if let Err(e) = speckit::exclude(&project) {
            log::warn!("cannot keep the Spec Kit folders out of git: {e:#}");
        }
    }
    Ok(())
}

/// `.paddock/` of the sandbox is the loft's notes folder. The project's
/// inbox stays visible at `.paddock/inbox/`. A project whose `.paddock` is
/// the host link to the loft sees the loft at the link's target.
fn notes(spec: &mut SandboxSpec, loft: &Loft, project: &Path, in_place: bool) -> Result<()> {
    let notes = loft.notes();
    crate::privfs::dir(&notes)?;
    let dst = project.join(NOTES);
    match std::fs::symlink_metadata(&dst) {
        Ok(m) if m.file_type().is_symlink() => {
            anyhow::ensure!(
                std::fs::read_link(&dst).ok().as_deref() == Some(notes.as_path()),
                "{} is a symlink that does not lead to the loft",
                dst.display()
            );
            spec.add_mount(Mount::same(&notes, true, "loft notes"));
        }
        Ok(m) if !m.is_dir() => anyhow::bail!("{} is not a folder", dst.display()),
        // The project's own `.paddock/` folder, in the project.
        _ if !in_place => {}
        _ => {
            let inbox = crate::broker::inbox(project)?;
            spec.add_mount(Mount::bind(&notes, &dst, true, "loft notes"));
            spec.add_mount(Mount::bind(
                inbox.path(),
                dst.join("inbox"),
                true,
                "host request inbox",
            ));
        }
    }
    Ok(())
}

/// Mount one private path of the loft. Only paths that the loft has are
/// mounted, at an empty mount point. Returns true when it mounted.
fn mount_entry(spec: &mut SandboxSpec, loft: &Loft, project: &Path, e: &Entry) -> Result<bool> {
    let src = loft.path(e);
    let Ok(sm) = std::fs::symlink_metadata(&src) else {
        return Ok(false);
    };
    if sm.is_dir() != e.is_dir() || sm.file_type().is_symlink() {
        log::warn!("{} in the loft has the wrong kind", src.display());
        return Ok(false);
    }
    let dst = project.join(e.rel());
    let root = crate::safefs::Root::open(project)?;
    let parent = e.rel().parent().unwrap_or(Path::new(""));
    let name = e.rel().file_name().unwrap_or_default();
    let here = match std::fs::symlink_metadata(&dst) {
        Ok(m) if e.is_dir() && m.is_dir() => {
            std::fs::read_dir(&dst).is_ok_and(|mut d| d.next().is_none())
        }
        Ok(m) if !e.is_dir() && m.is_file() => m.len() == 0,
        Ok(_) => false,
        Err(_) => {
            let dir = root.dir(parent, true)?;
            if e.is_dir() {
                dir.make_dir(name, 0o755)?;
            } else {
                drop(dir.create_new(name, 0o644)?);
            }
            true
        }
    };
    if !here {
        log::warn!(
            "{} is in the project and in the loft: paddock does not mount the loft over it (`paddock loft adopt` moves it)",
            dst.display()
        );
        return Ok(false);
    }
    spec.add_mount(Mount::bind(&src, &dst, true, "loft"));
    Ok(true)
}

/// The end of a run: one commit of the loft, unless the user commits.
pub fn end_of_run(env: &HostEnv, project: &Path, id: &str, harness: &str) {
    let loft = Loft::of(env, project);
    if !loft.active() || Cfg::load(env).commit == Commit::Manual {
        return;
    }
    if let Err(e) = git::commit(&loft, &format!("Run {id} ({harness})")) {
        log::warn!("cannot commit the loft {}: {e:#}", loft.root.display());
    }
}

#[cfg(test)]
mod tests;
