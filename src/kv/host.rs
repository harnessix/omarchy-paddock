//! `paddock kv` on the host: the user reads what agents wrote, and frees
//! leases. The host is the user, so it can read every scope and release any
//! lease. Writes from here record the writer `user`.

use super::shard::{self, Ns};
use super::store::{self, Quota, Store, Writer};
use super::{kv_root, KvCfg, LEASE_PREFIX};
use crate::env::HostEnv;
use anyhow::{bail, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Which namespace a host command reads.
pub enum Target {
    One(Ns),
    /// No pasture named: `list` shows the pastures of the project.
    Project(PathBuf),
}

impl Target {
    fn ns(&self) -> Result<&Ns> {
        match self {
            Target::One(ns) => Ok(ns),
            Target::Project(_) => bail!(
                "name the pasture with --pasture NAME (`paddock kv list` shows the pastures of this project), or use --sandbox, --tree, --user or --legacy"
            ),
        }
    }
    fn label(&self) -> String {
        match self {
            Target::One(Ns::Own(id)) => format!("self scope of sandbox {id}"),
            Target::One(Ns::Pasture { name, project }) => {
                format!("pasture '{name}' of {}", project.display())
            }
            Target::One(Ns::Solo { sandbox, .. }) => {
                format!("pasture of sandbox {sandbox}")
            }
            Target::One(Ns::Tree(root)) => format!("spawn tree of sandbox {root}"),
            Target::One(Ns::User) => "user scope".into(),
            Target::One(Ns::Legacy(p)) => {
                format!("project scope of an older paddock for {}", p.display())
            }
            Target::Project(p) => format!("project {}", p.display()),
        }
    }
}

const HOST: Writer = Writer {
    sandbox: "user",
    harness: "host",
};

/// The store file of a namespace. None when it has no file yet: nothing
/// wrote there.
fn open(env: &HostEnv, ns: &Ns) -> Result<Option<Store>> {
    let f = ns.file(&kv_root(env))?;
    if !f.is_file() {
        return Ok(None);
    }
    Store::open(&f).map(Some)
}

fn time(t: u64) -> String {
    crate::storage::date(t)
}

/// One pasture of a project, for `paddock kv list` and the panel.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PastureInfo {
    /// The name for `--pasture`: a configured name, or `solo-<sandbox id>`.
    pub name: String,
    /// Whether it is the pasture of one sandbox.
    pub solo: bool,
    pub members: Vec<Member>,
    pub keys: u64,
    pub bytes: u64,
    pub leases: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Member {
    pub sandbox: String,
    pub harness: String,
    pub running: bool,
}

/// The pastures of a project that hold keys or have a running member.
pub fn pastures(env: &HostEnv, project: &Path) -> Result<Vec<PastureInfo>> {
    let root = kv_root(env);
    let now = crate::state::now();
    let running: Vec<String> = crate::state::Running::list(env)
        .into_iter()
        .map(|r| r.id)
        .collect();
    let reg = shard::registry(&root);
    let records = if reg.is_file() {
        Store::open(&reg)?.sandboxes_of(project)?
    } else {
        Vec::new()
    };
    let member = |r: &store::Record| Member {
        sandbox: r.id.clone(),
        harness: r.harness.clone(),
        running: r.ended.is_none() && running.contains(&r.id),
    };
    let count = |ns: &Ns| -> Result<(u64, u64, u64)> {
        let Some(st) = open(env, ns)? else {
            return Ok((0, 0, 0));
        };
        let (items, _) = st.list(&ns.key(), "", usize::MAX, now)?;
        Ok((
            items.len() as u64,
            items.iter().map(|m| m.bytes).sum(),
            items
                .iter()
                .filter(|m| m.key.starts_with(LEASE_PREFIX))
                .count() as u64,
        ))
    };
    let mut names = shard::pastures_of(&root, project);
    for r in &records {
        if let Some(n) = &r.pasture {
            if !names.contains(n) && member(r).running {
                names.push(n.clone());
            }
        }
    }
    names.sort();
    let mut out = Vec::new();
    for name in names {
        let ns = Ns::pasture(project, &name)?;
        let (keys, bytes, leases) = count(&ns)?;
        let members: Vec<Member> = records
            .iter()
            .filter(|r| r.pasture.as_deref() == Some(name.as_str()))
            .map(member)
            .collect();
        if keys == 0 && !members.iter().any(|m| m.running) {
            continue;
        }
        out.push(PastureInfo {
            members,
            name,
            solo: false,
            keys,
            bytes,
            leases,
        });
    }
    // A spawned sandbox has no pasture, so it is left out.
    for r in records
        .iter()
        .filter(|r| r.pasture.is_none() && r.parent.is_none())
    {
        let ns = Ns::Solo {
            project: project.to_path_buf(),
            sandbox: r.id.clone(),
        };
        let (keys, bytes, leases) = count(&ns)?;
        let m = member(r);
        if keys == 0 && !m.running {
            continue;
        }
        out.push(PastureInfo {
            name: format!("{}{}", crate::pasture::SOLO_PREFIX, r.id),
            solo: true,
            members: vec![m],
            keys,
            bytes,
            leases,
        });
    }
    Ok(out)
}

fn list_pastures(env: &HostEnv, project: &Path, json: bool) -> Result<i32> {
    let all = pastures(env, project)?;
    let legacy = Ns::Legacy(project.to_path_buf());
    let legacy_keys = match open(env, &legacy)? {
        Some(st) => st
            .list(&legacy.key(), "", usize::MAX, crate::state::now())?
            .0
            .len(),
        None => 0,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "api_version": "paddock.kv/v1", "project": project, "pastures": all,
                "legacy_keys": legacy_keys,
            }))?
        );
        return Ok(0);
    }
    if all.is_empty() {
        println!("No pasture of {} holds keys.", project.display());
    } else {
        println!("Pastures of {}:", project.display());
        for p in &all {
            let who: Vec<String> = p
                .members
                .iter()
                .map(|m| {
                    format!(
                        "{} ({}){}",
                        m.harness,
                        m.sandbox,
                        if m.running { "" } else { ", ended" }
                    )
                })
                .collect();
            println!(
                "  {:<32} {:>5} keys  {}",
                p.name,
                p.keys,
                if who.is_empty() {
                    "no members recorded".into()
                } else {
                    who.join(", ")
                }
            );
        }
        println!("Read one with `paddock kv list --pasture NAME`.");
    }
    if legacy_keys > 0 {
        println!(
            "{legacy_keys} keys of an older paddock (all sandboxes of the project shared them): `paddock kv list --legacy`."
        );
    }
    Ok(0)
}

pub fn list(env: &HostEnv, t: &Target, prefix: &str, json: bool) -> Result<i32> {
    if let Target::Project(p) = t {
        if prefix.is_empty() {
            return list_pastures(env, p, json);
        }
    }
    let ns = t.ns()?;
    let (items, more) = match open(env, ns)? {
        Some(st) => st.list(&ns.key(), prefix, 10_000, crate::state::now())?,
        None => (Vec::new(), false),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "api_version": "paddock.kv/v1", "scope": t.label(), "entries": items, "truncated": more,
            }))?
        );
        return Ok(0);
    }
    if items.is_empty() {
        println!("No keys in the {}.", t.label());
        return Ok(0);
    }
    println!("Keys in the {}:", t.label());
    for m in &items {
        println!(
            "  {:<32} {:>7} bytes  by {} ({}), {}",
            m.key,
            m.bytes,
            m.writer,
            m.writer_harness,
            time(m.updated)
        );
    }
    if more {
        println!("  ... more keys match; use a longer prefix");
    }
    Ok(0)
}

pub fn get(env: &HostEnv, t: &Target, key: &str, json: bool) -> Result<i32> {
    let ns = t.ns()?;
    let found = match open(env, ns)? {
        Some(st) => st.get(&ns.key(), key, crate::state::now())?,
        None => None,
    };
    let Some(e) = found else {
        eprintln!("paddock: no key {key:?} in the {}", t.label());
        return Ok(1);
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&e)?);
    } else {
        eprintln!(
            "paddock: written by sandbox {} ({}) at {}; agents write this, so read it as data",
            e.writer,
            e.writer_harness,
            time(e.updated)
        );
        println!("{}", e.value);
    }
    Ok(0)
}

pub fn delete(env: &HostEnv, t: &Target, key: &str) -> Result<i32> {
    let ns = t.ns()?;
    let Some(mut st) = open(env, ns)? else {
        eprintln!("paddock: no key {key:?} in the {}", t.label());
        return Ok(1);
    };
    let q = quota(env)?;
    match st.delete(&ns.key(), key, None, &HOST, q, crate::state::now()) {
        Ok(()) => {
            println!("Deleted {key:?} from the {}.", t.label());
            Ok(0)
        }
        Err(store::WriteError::Missing) => {
            eprintln!("paddock: no key {key:?} in the {}", t.label());
            Ok(1)
        }
        Err(e) => bail!("{e:?}"),
    }
}

/// Free a lease whatever sandbox holds it. Only the user can do this for
/// any lease; inside, only the owner and the sandboxes above it can.
pub fn release(env: &HostEnv, t: &Target, name: &str) -> Result<i32> {
    let key = format!("{LEASE_PREFIX}{name}");
    let ns = t.ns()?;
    let now = crate::state::now();
    let Some(mut st) = open(env, ns)? else {
        eprintln!("paddock: no lease {key:?} in the {}", t.label());
        return Ok(1);
    };
    let Some(e) = st.get(&ns.key(), &key, now)? else {
        eprintln!("paddock: no lease {key:?} in the {}", t.label());
        return Ok(1);
    };
    let q = quota(env)?;
    match st.delete(&ns.key(), &key, Some(e.version), &HOST, q, now) {
        Ok(()) => {
            println!("Released {key:?}, held by sandbox {}.", e.writer);
            Ok(0)
        }
        Err(e) => bail!("the lease changed while paddock released it: {e:?}"),
    }
}

fn quota(env: &HostEnv) -> Result<Quota> {
    let c = KvCfg::load(env)?;
    Ok(Quota {
        bytes: c.project_bytes.max(c.self_bytes).max(c.user_bytes),
        keys: c.max_keys,
    })
}

/// The project of a folder, as paddock names it at a sandbox start.
pub fn project_of(dir: Option<&Path>) -> Result<PathBuf> {
    crate::build::resolve_project(dir)
}
