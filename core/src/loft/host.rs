//! The loft on the host: `paddock loft init|remove|adopt|status`, the
//! `.paddock` link in the project, `paddock explain`, and the setup step.

use super::{entries, exclude, exclude_line, external, git, Cfg, Entry, Kind, Loft, NOTES};
use crate::env::HostEnv;
use crate::speckit::{self, Store};
use anyhow::{bail, Context as _, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The repository of `dir`: its main work tree. A linked worktree is
/// refused for the commands that move files.
fn main_tree(dir: &Path) -> Result<(PathBuf, Option<crate::hostgit::Layout>)> {
    let layout = crate::hostgit::Layout::discover(dir)?;
    let repo = match &layout {
        Some(l) if l.linked => bail!(
            "{} is a worktree: run this in the main work tree",
            dir.display()
        ),
        Some(l) => l.work_tree.clone(),
        None => dir.to_path_buf(),
    };
    Ok((repo, layout))
}

/// Make the loft of the repository of `dir`, turn it on, and link
/// `.paddock` of the project to its notes. Returns what the user must know.
pub fn init(env: &HostEnv, dir: &Path) -> Result<String> {
    let repo = speckit::repo_of(dir);
    let loft = Loft::of(env, &repo);
    git::init(&loft)?;
    crate::privfs::dir(&loft.notes())?;
    let _ = std::fs::remove_file(loft.root.join("off"));
    let mut msg = format!(
        "The loft of {} is {}. Every sandbox of this repository keeps its notes and private files there, with a history of their own (`paddock loft log`).",
        repo.display(),
        loft.root.display()
    );
    match link(&loft, &repo) {
        Ok(()) => msg.push_str(&format!(
            "\n{}/{NOTES} leads to the loft's notes, also outside a sandbox.",
            repo.display()
        )),
        Err(e) => msg.push_str(&format!("\nNo link {NOTES} in the project: {e:#}")),
    }
    exclude(&repo, "the loft", &[format!("/{NOTES}")])?;
    let waiting = waiting(env, &Cfg::load(env), &loft, &repo);
    if !waiting.is_empty() {
        msg.push_str(&format!(
            "\nThe project still has {}: `paddock loft adopt` moves them into the loft.",
            waiting.join(", ")
        ));
    }
    Ok(msg)
}

/// `.paddock` of the project becomes a symlink to the loft's notes. A
/// `.paddock` folder moves into the notes first; the host request inbox
/// moves along. Nothing in the project is followed.
pub fn link(loft: &Loft, repo: &Path) -> Result<()> {
    let notes = loft.notes();
    let root = crate::safefs::Root::open(repo)?.dir(Path::new(""), false)?;
    let name = OsStr::new(NOTES);
    match root.kind(name)? {
        None => {}
        Some(crate::safefs::Kind::Symlink) => {
            if root.read_link(name)? == notes {
                return Ok(());
            }
            bail!("{NOTES} is a symlink to another place; remove it first");
        }
        Some(crate::safefs::Kind::Dir) => {
            // rename does not follow the last component: whatever is there
            // moves into a folder of the host.
            let staging = loft
                .root
                .join(format!("from-project-{}", crate::state::now()));
            std::fs::rename(repo.join(NOTES), &staging).with_context(|| {
                format!(
                    "cannot move {}/{NOTES} into the loft; move it aside",
                    repo.display()
                )
            })?;
            if !std::fs::symlink_metadata(&staging).is_ok_and(|m| m.is_dir()) {
                let _ = std::fs::remove_file(&staging);
            } else {
                for e in std::fs::read_dir(&staging)?.flatten() {
                    let n = e.file_name();
                    if n == ".gitignore" {
                        let _ = std::fs::remove_file(e.path());
                    } else if !notes.join(&n).exists() {
                        std::fs::rename(e.path(), notes.join(&n))?;
                    }
                }
                if std::fs::remove_dir(&staging).is_err() {
                    log::warn!(
                        "files of the old {NOTES} that the notes have already stay in {}",
                        staging.display()
                    );
                }
            }
        }
        Some(_) => bail!("{NOTES} is not a folder"),
    }
    root.symlink(&notes, name)
}

/// The `.paddock` link of the project leads to its loft.
pub fn linked(env: &HostEnv, project: &Path) -> bool {
    let notes = Loft::of(env, project).notes();
    std::fs::symlink_metadata(project.join(NOTES)).is_ok_and(|m| m.file_type().is_symlink())
        && std::fs::read_link(project.join(NOTES)).ok().as_deref() == Some(notes.as_path())
}

/// Take the link away and turn the loft off. Its files and history stay.
pub fn remove(env: &HostEnv, dir: &Path) -> Result<String> {
    let repo = speckit::repo_of(dir);
    let loft = Loft::of(env, &repo);
    if !loft.initialized() {
        bail!("{} has no loft", repo.display());
    }
    if linked(env, &repo) {
        crate::safefs::Root::open(&repo)?
            .dir(Path::new(""), false)?
            .remove(OsStr::new(NOTES))?;
    }
    crate::privfs::write(&loft.root.join("off"), b"")?;
    Ok(format!(
        "The loft is off. Its files and history stay in {}. `paddock loft init` turns it on again.",
        loft.root.display()
    ))
}

/// A private path in the project: a file or a folder with files, not the
/// empty mount point of a run. Every folder on the way is a real folder.
fn in_project(repo: &Path, e: &Entry) -> bool {
    let mut at = repo.to_path_buf();
    for c in e.rel().parent().into_iter().flat_map(|p| p.components()) {
        at.push(c);
        if !std::fs::symlink_metadata(&at).is_ok_and(|m| m.is_dir()) {
            return false;
        }
    }
    let p = repo.join(e.rel());
    match std::fs::symlink_metadata(&p) {
        Ok(m) if e.is_dir() && m.is_dir() => {
            std::fs::read_dir(&p).is_ok_and(|mut d| d.next().is_some())
        }
        Ok(m) if !e.is_dir() && m.is_file() => m.len() > 0,
        _ => false,
    }
}

/// The private paths in the project that adopt would move.
fn waiting(env: &HostEnv, cfg: &Cfg, loft: &Loft, repo: &Path) -> Vec<String> {
    let mut out: Vec<String> = entries(cfg)
        .iter()
        .filter(|e| external(cfg, true, e) && in_project(repo, e) && !loft.path(e).exists())
        .map(|e| e.path.clone())
        .collect();
    let sk = speckit::Cfg::load(env);
    if sk.enabled
        && speckit::Layout::resolve_with(env, &sk, repo).is_some_and(|l| !l.external)
        && super::speckit_external(env, &sk, repo)
    {
        out.push(format!("{}/", speckit::SPECIFY));
    }
    out
}

/// Move the private files of the project into the loft, after a question.
/// It never rewrites git history: it says how to drop tracked files.
pub fn adopt(env: &HostEnv, dir: &Path, ask: &mut dyn FnMut(&str) -> bool) -> Result<String> {
    let (repo, layout) = main_tree(dir)?;
    let cfg = Cfg::load(env);
    if cfg.store != Store::External {
        bail!("[loft] store is \"repo\": the private paths stay in the repository");
    }
    let loft = Loft::of(env, &repo);
    if !loft.active() {
        bail!(
            "{} has no loft: run `paddock loft init` first",
            repo.display()
        );
    }
    let list: Vec<Entry> = entries(&cfg)
        .into_iter()
        .filter(|e| external(&cfg, true, e) && in_project(&repo, e) && !loft.path(e).exists())
        .collect();
    let all = waiting(env, &cfg, &loft, &repo);
    if all.is_empty() {
        bail!("{} has no private files to move", repo.display());
    }
    if !ask(&format!(
        "Move {} of {} into the loft {}? Every sandbox of this repository sees them in place.",
        all.join(", "),
        repo.display(),
        loft.root.display()
    )) {
        return Ok("Nothing moved.".into());
    }
    let paths: Vec<String> = list.iter().map(|e| e.rel().display().to_string()).collect();
    for e in &list {
        let (from, to) = (repo.join(e.rel()), loft.path(e));
        if let Some(p) = to.parent() {
            crate::privfs::dir(p)?;
        }
        if e.is_dir() {
            speckit::adopt::move_dir(&from, &to)?;
        } else if std::fs::rename(&from, &to).is_err() {
            std::fs::copy(&from, &to)?;
            std::fs::remove_file(&from)?;
        }
    }
    let lines: Vec<String> = list.iter().map(exclude_line).collect();
    exclude(&repo, "the loft", &lines)?;
    let mut msg = String::new();
    if !list.is_empty() {
        msg = format!(
            "Moved {} into {}. paddock mounts them into each sandbox of this repository, and info/exclude keeps them out of git.",
            paths.join(", "),
            loft.tree().display()
        );
        let mut q = vec!["ls-files", "--"];
        q.extend(paths.iter().map(String::as_str));
        let tracked = layout
            .as_ref()
            .and_then(|l| l.try_query(&q))
            .is_some_and(|o| !o.trim().is_empty());
        if tracked {
            let invert: Vec<String> = paths.iter().map(|p| format!("--path {p}")).collect();
            msg.push_str(&format!(
                "\n\nSome of these files were in git. Remove them from the index (the files stay in the loft):\n  git rm -r -q --cached {}\n  git commit -m \"Move the private files out of the repository\"\nThey stay in the history. To remove them from it, rewrite the history yourself, for example with\n  git filter-repo --invert-paths {}\nThat changes every commit id, so do it only on a repository whose other copies you can replace.",
                paths.join(" "),
                invert.join(" ")
            ));
        }
    }
    if all.len() > list.len() {
        if !msg.is_empty() {
            msg.push_str("\n\n");
        }
        msg.push_str(&speckit::adopt::adopt(env, &repo, &mut |_| true)?);
    }
    git::commit(&loft, "Adopt the private files of the project")?;
    Ok(msg)
}

/// `paddock loft status`: the state, the files that wait for adopt, and
/// the changes since the last commit.
pub fn status(env: &HostEnv, dir: &Path) -> Result<String> {
    let repo = speckit::repo_of(dir);
    let loft = Loft::of(env, &repo);
    let cfg = Cfg::load(env);
    let state = if !loft.initialized() {
        "not made (`paddock loft init` makes it)"
    } else if loft.off() {
        "off (`paddock loft init` turns it on)"
    } else {
        "on"
    };
    let mut out = format!(
        "loft      {}\nstate     {state}\nstore     {}\nlink      {}\n",
        loft.root.display(),
        if cfg.store == Store::External {
            "external"
        } else {
            "repo"
        },
        if linked(env, &repo) {
            format!("{}/{NOTES} leads to the loft's notes", repo.display())
        } else {
            "none".into()
        }
    );
    let waiting = waiting(env, &cfg, &loft, &repo);
    if loft.active() && !waiting.is_empty() {
        out.push_str(&format!(
            "waiting   {} (`paddock loft adopt` moves them)\n",
            waiting.join(", ")
        ));
    }
    if loft.initialized() {
        let s = git::status(&loft)?;
        out.push_str(if s.is_empty() {
            "changes   none since the last commit\n"
        } else {
            "changes   since the last commit:\n"
        });
        out.push_str(&s);
    }
    Ok(out)
}

/// `paddock explain`: each private path, where it lives, and whether git
/// tracks it.
pub fn explain(env: &HostEnv, project: &Path) -> Vec<String> {
    let cfg = Cfg::load(env);
    let loft = Loft::of(env, project);
    let active = loft.active();
    let sk = speckit::Cfg::load(env);
    let tracked: Vec<String> = crate::hostgit::Layout::discover(project)
        .ok()
        .flatten()
        .and_then(|l| {
            l.try_query(&["ls-files", "--full-name", "--", "."])
                .map(|o| (l, o))
        })
        .map(|(l, o)| {
            let rel = project.strip_prefix(&l.work_tree).unwrap_or(Path::new(""));
            let pre = format!("{}/", rel.display());
            o.lines()
                .map(|n| n.strip_prefix(pre.as_str()).unwrap_or(n).to_string())
                .collect()
        })
        .unwrap_or_default();
    let mut out = vec![format!(
        "  notes   {NOTES}/  <- {}\n         the agents' notes, outside the repository",
        loft.notes().display()
    )];
    for e in entries(&cfg) {
        let in_loft = if e.kind == Kind::SpecKit {
            super::speckit_external(env, &sk, project) && loft.path(&e).exists()
        } else {
            external(&cfg, active, &e) && loft.path(&e).exists()
        };
        let place = if e.public {
            "public: kept in git on purpose".to_string()
        } else if in_loft {
            format!("in the loft: {}", loft.path(&e).display())
        } else if std::fs::symlink_metadata(project.join(e.rel())).is_ok() {
            "in the project".to_string()
        } else {
            "absent".to_string()
        };
        let git = if tracked.iter().any(|n| e.matches(n)) {
            "git tracks it"
        } else {
            "git does not track it"
        };
        out.push(format!("  {:<28} {place}, {git}", e.path));
    }
    out
}

/// `paddock setup`: offer `[loft] auto = true`.
pub fn setup_step(
    env: &HostEnv,
    flag: Option<bool>,
    ask: &mut Option<&mut dyn crate::setup::Ask>,
    out: &mut crate::setup::Outcome,
) -> Result<()> {
    println!("\n8. The loft");
    println!("   Agents leave design notes and instruction files in repositories, and a push publishes them.\n   The loft keeps them in a private store above each project, with a history of its own.\n   It sets [loft] auto = true in the configuration. Undo: paddock setup --no-loft");
    let on = Cfg::load(env).auto;
    let want = match (flag, on) {
        (Some(f), _) => Some(f),
        (None, true) => None,
        (None, false) => ask
            .as_mut()
            .map(|a| a.yes_no("Make a loft for every project?", true)),
    };
    match (on, want) {
        (false, Some(true)) | (true, Some(false)) => {
            let v = want == Some(true);
            set_auto(env, v)?;
            println!("   {}", if v { "on" } else { "off" });
            out.changes.push(format!(
                "turned {} the loft for every project",
                if v { "on" } else { "off" }
            ));
        }
        (true, _) => println!("   on       already on"),
        (false, _) => println!("   off      not changed; turn on with: paddock setup --loft"),
    }
    Ok(())
}

fn set_auto(env: &HostEnv, on: bool) -> Result<()> {
    let file = env.paddock_config().join("config.toml");
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("{} is invalid", file.display()))?;
    if !doc.contains_table("loft") {
        doc.insert("loft", toml_edit::table());
    }
    doc["loft"]["auto"] = toml_edit::value(on);
    std::fs::create_dir_all(env.paddock_config())?;
    std::fs::write(&file, doc.to_string())?;
    Ok(())
}
