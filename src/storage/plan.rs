use super::*;
use crate::state::AuthMode;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct GcOptions {
    pub yes: bool,
    pub homes: bool,
    pub older_than_days: u64,
    pub include_app_logins: bool,
    pub worktrees: bool,
    pub force: bool,
    /// Also scratch projects that were not used for `older_than_days`.
    pub scratch: bool,
    /// Also share the data of identical files in the sandbox homes.
    pub dedupe: bool,
    /// Also project snapshots and undo trash folders older than
    /// `older_than_days`.
    pub snapshots: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Safe to remove without a question: `--yes` alone.
    Auto,
    /// Needs a flag that names it, such as `--homes`.
    Consent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Action {
    Image {
        cli: String,
        tags: Vec<String>,
    },
    BuildCache {
        cli: String,
        ids: Vec<String>,
    },
    Path {
        path: PathBuf,
    },
    Worktree {
        path: PathBuf,
        repo: Option<PathBuf>,
        branch: Option<String>,
        /// Delete the branch too: it has no commits of its own.
        delete_branch: bool,
        /// Remove with uncommitted changes.
        force: bool,
    },
    /// paddock kv: the `self` data of these finished runs, and expired keys.
    Kv {
        sandboxes: Vec<String>,
    },
    /// The live view of an ended sandbox: its tmux session and run log
    /// (view.rs).
    View {
        id: String,
    },
    /// Pure cache folders in a home, relative to it (storage/caches.rs).
    Caches {
        home: PathBuf,
        folders: Vec<PathBuf>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub what: String,
    pub tier: Tier,
    pub bytes: u64,
    pub reason: String,
    pub warning: Option<String>,
    /// Flags still missing before `--yes` removes it. Empty: selected.
    pub needs: Vec<String>,
    pub action: Action,
}

impl Item {
    pub fn selected(&self) -> bool {
        self.needs.is_empty()
    }
}

fn need(v: &mut Vec<String>, have: bool, flag: &str) {
    if !have {
        v.push(flag.to_string());
    }
}

fn home_label(h: &Home) -> String {
    format!(
        "home {}/{}/{}",
        h.harness,
        login_dir(&h.auth.parse().unwrap_or(AuthMode::Shared)),
        name(&h.path)
    )
}

/// Decide what gc removes. Pure: everything comes from the inventory.
pub fn plan(inv: &Inventory, o: &GcOptions, now: u64) -> Vec<Item> {
    let old = now.saturating_sub(o.older_than_days * DAY);
    let mut items = Vec::new();
    plan_layers(&mut items, inv);
    plan_old_caches(&mut items, inv);
    plan_images(&mut items, inv);
    plan_build_cache(&mut items, inv);
    plan_keyrings(&mut items, inv);
    plan_sessions(&mut items, inv, now);
    plan_homes(&mut items, inv, o, now, old);
    plan_snapshots(&mut items, inv, o, now, old);
    plan_pure_caches(&mut items, inv);
    plan_scratch(&mut items, inv, o, now, old);
    plan_worktrees(&mut items, inv, o);
    items
}

fn plan_layers(items: &mut Vec<Item>, inv: &Inventory) {
    for l in inv.layers.iter().filter(|l| l.orphan) {
        let project = l
            .project
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "unknown".into());
        items.push(Item {
            what: format!("cache layers {}", l.path.display()),
            tier: Tier::Auto,
            bytes: l.bytes,
            reason: format!("its project {project} no longer exists"),
            warning: None,
            needs: vec![],
            action: Action::Path {
                path: l.path.clone(),
            },
        });
    }
}

fn plan_old_caches(items: &mut Vec<Item>, inv: &Inventory) {
    for c in &inv.old_caches {
        if inv.any_running {
            continue;
        }
        items.push(Item {
            what: format!("old cache {}", c.path.display()),
            tier: Tier::Auto,
            bytes: c.bytes,
            reason: format!("paddock no longer uses it; it uses {} now", c.replaced_by),
            warning: None,
            needs: vec![],
            action: Action::Path {
                path: c.path.clone(),
            },
        });
    }
}

fn plan_images(items: &mut Vec<Item>, inv: &Inventory) {
    for i in &inv.images {
        if i.current || i.in_use {
            continue;
        }
        items.push(Item {
            what: format!("image {} ({})", i.tags.join(", "), i.cli),
            tier: Tier::Auto,
            // Images share layers: only the unique part is freed.
            bytes: i.unique.unwrap_or(i.bytes),
            reason: "not a current paddock image, and no container uses it".into(),
            warning: None,
            needs: vec![],
            action: Action::Image {
                cli: i.cli.clone(),
                tags: i.tags.clone(),
            },
        });
    }
}

fn plan_build_cache(items: &mut Vec<Item>, inv: &Inventory) {
    // paddock's base image builds in the docker build cache. They hold the
    // layers of removed images; current images keep their own layers, so a
    // removed record only costs time at the next build.
    let mut by_cli: BTreeMap<&str, Vec<&BuildRecord>> = BTreeMap::new();
    for r in &inv.build_cache {
        by_cli.entry(&r.cli).or_default().push(r);
    }
    for (cli, recs) in by_cli {
        items.push(Item {
            what: format!("paddock build cache ({cli}, {} records)", recs.len()),
            tier: Tier::Auto,
            bytes: recs.iter().map(|r| r.bytes).sum(),
            reason: "build steps of paddock base images; images keep their own layers".into(),
            warning: None,
            needs: vec![],
            action: Action::BuildCache {
                cli: cli.to_string(),
                ids: recs.iter().map(|r| r.id.clone()).collect(),
            },
        });
    }
}

fn plan_keyrings(items: &mut Vec<Item>, inv: &Inventory) {
    for k in inv.keyrings.iter().filter(|k| !k.home_exists) {
        items.push(Item {
            what: format!("keyring password {}", name(&k.path)),
            tier: Tier::Auto,
            bytes: k.bytes,
            reason: "its sandbox home is gone".into(),
            warning: None,
            needs: vec![],
            action: Action::Path {
                path: k.path.clone(),
            },
        });
    }
}

fn plan_sessions(items: &mut Vec<Item>, inv: &Inventory, now: u64) {
    let cutoff = now.saturating_sub(SESSION_DAYS * DAY);
    for s in &inv.sessions {
        // A sandbox that crashed never wrote its end: its start counts.
        let end = s.ended.unwrap_or(s.started);
        if s.running || end >= cutoff {
            continue;
        }
        items.push(Item {
            what: format!("session record {}", s.id),
            tier: Tier::Auto,
            bytes: s.bytes,
            reason: format!("ended more than {SESSION_DAYS} days ago"),
            warning: None,
            needs: vec![],
            action: Action::Path {
                path: s.path.clone(),
            },
        });
    }
}

fn plan_homes(items: &mut Vec<Item>, inv: &Inventory, o: &GcOptions, now: u64, old: u64) {
    for h in &inv.homes {
        if h.running {
            continue;
        }
        let login = h
            .app
            .then(|| "the app's login in this home will be lost".to_string());
        let mut needs = Vec::new();
        let (tier, reason) = if h.harness == DOCTOR_HARNESS {
            // Each escape run has its own scratch project and home. A home
            // whose project is gone is a leftover; one without a verified
            // origin is one when it is a day old. A run that is still
            // setting up has its project and is not touched.
            let left = h.project_exists == Some(false)
                || (h.project.is_none() && h.last_active() < now.saturating_sub(DAY));
            if !left {
                continue;
            }
            (
                Tier::Auto,
                "a leftover of an escape test (paddock doctor --escape)".to_string(),
            )
        } else if !h.profile {
            if h.app {
                need(&mut needs, o.homes, "--homes");
                need(&mut needs, o.include_app_logins, "--include-app-logins");
            }
            let tier = if h.app { Tier::Consent } else { Tier::Auto };
            (
                tier,
                format!("the harness profile '{}' no longer exists", h.harness),
            )
        } else if h.project_exists == Some(false) {
            need(&mut needs, o.homes, "--homes");
            if h.app {
                need(&mut needs, o.include_app_logins, "--include-app-logins");
            }
            let p = h.project.as_ref().map(|p| p.display().to_string());
            (
                Tier::Consent,
                format!("the project {} is gone", p.unwrap_or_default()),
            )
        } else if h.last_active() < old {
            need(&mut needs, o.homes, "--homes");
            if h.app {
                need(&mut needs, o.include_app_logins, "--include-app-logins");
            }
            (
                Tier::Consent,
                format!(
                    "not used for {} days",
                    now.saturating_sub(h.last_active()) / DAY
                ),
            )
        } else {
            continue;
        };
        items.push(Item {
            what: home_label(h),
            tier,
            bytes: h.bytes,
            reason,
            warning: login,
            needs,
            action: Action::Path {
                path: h.path.clone(),
            },
        });
    }
}

fn plan_snapshots(items: &mut Vec<Item>, inv: &Inventory, o: &GcOptions, now: u64, old: u64) {
    for c in &inv.snapshots {
        let gone = !c.trash && !c.project_exists;
        if !gone && c.started > old {
            continue;
        }
        let mut needs = Vec::new();
        if !gone {
            need(&mut needs, o.snapshots, "--snapshots");
        }
        let what = match &c.project {
            Some(p) if !c.trash => format!("snapshot {} of {}", name(&c.path), p.display()),
            _ => format!("undo trash {}", c.path.display()),
        };
        items.push(Item {
            what,
            tier: if gone { Tier::Auto } else { Tier::Consent },
            bytes: c.bytes,
            reason: if gone {
                "its project is gone".into()
            } else {
                format!("{} days old", now.saturating_sub(c.started) / DAY)
            },
            warning: c
                .trash
                .then(|| "the files that an undo moved away are lost".to_string()),
            needs,
            action: Action::Path {
                path: c.path.clone(),
            },
        });
    }
}

fn plan_pure_caches(items: &mut Vec<Item>, inv: &Inventory) {
    // Pure caches rebuild themselves. A home that this plan removes
    // without a question needs no item of its own.
    let gone: BTreeSet<PathBuf> = items
        .iter()
        .filter(|i| i.tier == Tier::Auto)
        .filter_map(|i| match &i.action {
            Action::Path { path } => Some(path.clone()),
            _ => None,
        })
        .collect();
    for h in &inv.homes {
        if !inv.tmpfs_caches || h.running || gone.contains(&h.path) {
            continue;
        }
        let folders: Vec<PathBuf> = h
            .caches
            .iter()
            .filter(|(c, _)| *c == caches::Class::Scratch)
            .map(|(_, r)| r.clone())
            .collect();
        if folders.is_empty() {
            continue;
        }
        items.push(Item {
            what: format!("pure caches in {}", home_label(h)),
            tier: Tier::Auto,
            bytes: 0,
            reason: format!(
                "they rebuild themselves: {}",
                folders
                    .iter()
                    .map(|f| f.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            warning: None,
            needs: vec![],
            action: Action::Caches {
                home: h.path.clone(),
                folders,
            },
        });
    }
}

fn plan_scratch(items: &mut Vec<Item>, inv: &Inventory, o: &GcOptions, now: u64, old: u64) {
    for c in &inv.scratch {
        if c.running || c.last_active > old {
            continue;
        }
        let mut needs = Vec::new();
        need(&mut needs, o.scratch, "--scratch");
        items.push(Item {
            what: format!("scratch project {} {}", c.harness, c.path.display()),
            tier: Tier::Consent,
            bytes: c.bytes,
            reason: format!(
                "not used for {} days",
                now.saturating_sub(c.last_active) / DAY
            ),
            warning: (c.entries > 0)
                .then(|| format!("its {} files and folders are lost", c.entries)),
            needs,
            action: Action::Path {
                path: c.path.clone(),
            },
        });
    }
}

fn plan_worktrees(items: &mut Vec<Item>, inv: &Inventory, o: &GcOptions) {
    for w in &inv.worktrees {
        if w.running {
            continue;
        }
        let mut needs = Vec::new();
        need(&mut needs, o.worktrees, "--worktrees");
        let mut risks = Vec::new();
        if w.repo.is_none() {
            risks.push("paddock cannot find its repository, so it cannot check it".to_string());
        } else {
            match w.unmerged {
                Some(0) => {}
                Some(n) => risks.push(format!(
                    "{n} commits on no other branch (the branch is kept)"
                )),
                None => risks.push("paddock cannot count its unmerged commits".into()),
            }
            match w.dirty {
                Some(false) => {}
                Some(true) => risks.push("uncommitted changes will be lost".into()),
                None => risks.push("paddock cannot check for uncommitted changes".into()),
            }
        }
        let risky = !risks.is_empty();
        if risky {
            need(&mut needs, o.force, "--force");
        }
        let delete_branch =
            w.unmerged == Some(0) && w.branch.as_ref().is_some_and(|b| b.starts_with("paddock/"));
        items.push(Item {
            what: format!(
                "worktree {}{}",
                w.path.display(),
                w.branch
                    .as_ref()
                    .map(|b| format!(" ({b})"))
                    .unwrap_or_default()
            ),
            tier: Tier::Consent,
            bytes: w.bytes,
            reason: match &w.repo {
                Some(r) => format!("a paddock worktree of {}", r.display()),
                None => "a paddock worktree".into(),
            },
            warning: risky.then(|| risks.join("; ")),
            needs,
            action: Action::Worktree {
                path: w.path.clone(),
                repo: w.repo.clone(),
                branch: w.branch.clone(),
                delete_branch,
                force: risky,
            },
        });
    }
}

/// Measure only the homes and worktrees that the plan lists.
pub fn measure_plan(inv: &mut Inventory, items: &mut [Item]) {
    let listed: BTreeSet<PathBuf> = items
        .iter()
        .filter_map(|i| match &i.action {
            Action::Path { path } | Action::Worktree { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    let homes: BTreeSet<&PathBuf> = inv.homes.iter().map(|h| &h.path).collect();
    let wts: BTreeSet<&PathBuf> = inv
        .worktrees
        .iter()
        .map(|w| &w.path)
        .chain(inv.old_caches.iter().map(|c| &c.path))
        .chain(inv.layers.iter().map(|c| &c.path))
        .chain(inv.scratch.iter().map(|c| &c.path))
        .collect();
    let want: BTreeSet<PathBuf> = listed
        .into_iter()
        .filter(|p| homes.contains(p) || wts.contains(p))
        .collect();
    let cache_paths: Vec<PathBuf> = items
        .iter()
        .flat_map(|i| match &i.action {
            Action::Caches { home, folders } => folders.iter().map(|f| home.join(f)).collect(),
            _ => vec![],
        })
        .collect();
    if !cache_paths.is_empty() {
        let (v, _) = sizes(&cache_paths);
        let mut it = v.into_iter();
        for i in items.iter_mut() {
            if let Action::Caches { folders, .. } = &i.action {
                i.bytes = it.by_ref().take(folders.len()).sum();
            }
        }
    }
    if want.is_empty() {
        return;
    }
    measure(inv, |p| want.contains(p));
    let size: BTreeMap<&PathBuf, u64> = inv
        .homes
        .iter()
        .map(|h| (&h.path, h.bytes))
        .chain(inv.worktrees.iter().map(|w| (&w.path, w.bytes)))
        .chain(inv.old_caches.iter().map(|c| (&c.path, c.bytes)))
        .chain(inv.layers.iter().map(|c| (&c.path, c.bytes)))
        .chain(inv.scratch.iter().map(|c| (&c.path, c.bytes)))
        .collect();
    for i in items.iter_mut() {
        if let Action::Path { path } | Action::Worktree { path, .. } = &i.action {
            if let Some(b) = size.get(path) {
                i.bytes = *b;
            }
        }
    }
}
