//! `paddock kv` on the host: the user reads what agents wrote, and frees
//! leases. The host is the user, so it can read every scope and release any
//! lease. Writes from here record the writer `user`.

use super::store::{self, Quota, Store, Writer};
use super::{store_path, KvCfg, LEASE_PREFIX};
use crate::env::HostEnv;
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

/// Which namespace a host command reads.
pub enum Target {
    Project(PathBuf),
    User,
    Sandbox(String),
    /// The tree scope of a spawn tree, by the id of its root sandbox.
    Tree(String),
}

impl Target {
    pub fn ns(&self) -> String {
        match self {
            Target::Project(p) => store::ns_project(p),
            Target::User => store::NS_USER.into(),
            Target::Sandbox(id) => store::ns_self(id),
            Target::Tree(root) => store::ns_tree(root),
        }
    }
    fn label(&self) -> String {
        match self {
            Target::Project(p) => format!("project {}", p.display()),
            Target::User => "user scope".into(),
            Target::Sandbox(id) => format!("sandbox {id}"),
            Target::Tree(root) => format!("spawn tree of sandbox {root}"),
        }
    }
}

const HOST: Writer = Writer {
    sandbox: "user",
    harness: "host",
};

fn open(env: &HostEnv) -> Result<Store> {
    let p = store_path(env);
    if !p.exists() {
        bail!("the paddock kv store does not exist yet: no sandbox has started");
    }
    Store::open(&p)
}

fn time(t: u64) -> String {
    crate::storage::date(t)
}

pub fn list(env: &HostEnv, t: &Target, prefix: &str, json: bool) -> Result<i32> {
    let st = open(env)?;
    let (items, more) = st.list(&t.ns(), prefix, 10_000, crate::state::now())?;
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
    let st = open(env)?;
    let Some(e) = st.get(&t.ns(), key, crate::state::now())? else {
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
    let mut st = open(env)?;
    let q = quota(env)?;
    match st.delete(&t.ns(), key, None, &HOST, q, crate::state::now()) {
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
    let mut st = open(env)?;
    let now = crate::state::now();
    let Some(e) = st.get(&t.ns(), &key, now)? else {
        eprintln!("paddock: no lease {key:?} in the {}", t.label());
        return Ok(1);
    };
    let q = quota(env)?;
    match st.delete(&t.ns(), &key, Some(e.version), &HOST, q, now) {
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
