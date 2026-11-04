//! Named principals: a host script or a CI job on the same machine uses
//! paddock kv under a name from `[kv.principals.NAME]` of the user
//! configuration (`paddock kv put --as NAME --pasture P`).
//!
//! A principal goes through the same service as a sandbox, with an identity
//! that paddock makes on the host: the writer `principal:NAME`, the pasture
//! named on the command line. So it has the quotas of a sandbox writer, and
//! it can free only its own leases. Its first write to a pasture in a user
//! session asks the user on an approval card. "Allow for this session" is
//! kept in the run folder, which ends with the user's session.

use super::{audit_file, kv_root, KvCfg, Request, Response, Service, Status, WRITE_OPS};
use crate::broker::{Answer, Approver, Pending};
use crate::env::HostEnv;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The writer of the entries of a principal: `principal:NAME`.
pub const WRITER_PREFIX: &str = "principal:";
/// The harness field of the entries of a principal.
pub const HARNESS: &str = "principal";
/// The capability on the approval card.
pub const CAPABILITY: &str = "kv-principal";
/// The operations a principal can use.
pub const OPS: &[&str] = &["put", "delete", "watch", "claim", "release", "get", "list"];

/// `[kv.principals.NAME]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    /// The pastures of the current project that it may use.
    #[serde(default)]
    pub pastures: Vec<String>,
    /// false: it reads and watches only.
    #[serde(default = "yes")]
    pub write: bool,
}

fn yes() -> bool {
    true
}

/// Check the names of `[kv.principals]`.
pub(crate) fn check(all: BTreeMap<String, Principal>) -> Result<BTreeMap<String, Principal>> {
    for (name, p) in &all {
        let ok = !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok {
            bail!("principal '{name}': use 1 to 64 letters, digits, '-' and '_'");
        }
        for pasture in &p.pastures {
            crate::pasture::check_name(pasture)
                .map_err(|e| anyhow::anyhow!("principal '{name}': {e:#}"))?;
        }
    }
    Ok(all)
}

/// The session grants: principal and pasture of a project.
fn grants_file(env: &HostEnv) -> PathBuf {
    env.paddock_run().join("kv-principals.json")
}

type Grant = (String, String, PathBuf);

fn grants(env: &HostEnv) -> Vec<Grant> {
    std::fs::read_to_string(grants_file(env))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn granted(env: &HostEnv, name: &str, pasture: &str, project: &Path) -> bool {
    grants(env)
        .iter()
        .any(|(n, p, pr)| n == name && p == pasture && pr == project)
}

fn grant(env: &HostEnv, name: &str, pasture: &str, project: &Path) -> Result<()> {
    let mut all = grants(env);
    all.push((name.into(), pasture.into(), project.to_path_buf()));
    let f = grants_file(env);
    if let Some(dir) = f.parent() {
        crate::privfs::dir(dir)?;
    }
    crate::privfs::write_atomic(&f, serde_json::to_string(&all)?)?;
    Ok(())
}

/// One call of a principal.
pub struct Call<'a> {
    pub name: &'a str,
    pub project: &'a Path,
    pub pasture: &'a str,
    pub approver: Arc<dyn Approver>,
    pub wait: Duration,
    /// Another kv folder. Tests use it.
    pub root: Option<PathBuf>,
}

impl Call<'_> {
    fn writer(&self) -> String {
        format!("{WRITER_PREFIX}{}", self.name)
    }

    /// Log a decision that the service does not see.
    fn audit(&self, env: &HostEnv, req: &Request, decision: &str, status: Status) {
        let line = serde_json::json!({
            "time": crate::state::now(),
            "sandbox": self.writer(),
            "harness": HARNESS,
            "project": self.project,
            "principal": self.name,
            "pasture": self.pasture,
            "op": req.op,
            "scope": "pasture",
            "key": req.key.as_deref().or(req.prefix.as_deref()),
            "bytes": req.value.as_ref().map(|v| v.len()),
            "decision": decision,
            "status": status,
        });
        if let Ok(mut f) = crate::privfs::append(&audit_file(env)) {
            let _ = writeln!(f, "{line}");
        }
    }

    fn refuse(&self, env: &HostEnv, req: &Request, msg: String) -> Response {
        self.audit(env, req, "refused", Status::Denied);
        Response::new(Status::Denied, msg)
    }

    /// The first write of this principal to this pasture in the session
    /// asks the user. None: allowed.
    fn gate(&self, env: &HostEnv, req: &Request) -> Option<Response> {
        if granted(env, self.name, self.pasture, self.project) {
            return None;
        }
        let now = crate::state::now();
        let p = Pending {
            id: super::request_id(),
            sandbox: self.writer(),
            harness: HARNESS.into(),
            project: self.project.to_path_buf(),
            capability: CAPABILITY.into(),
            summary: format!("let `{}` write to the pasture `{}`", self.name, self.pasture),
            detail: format!(
                "The host principal `{}` from [kv.principals] of the user configuration wants to write to the pasture `{}` of {}. The agents of the pasture read what it writes, with the writer {}.",
                self.name,
                self.pasture,
                self.project.display(),
                self.writer()
            ),
            reason: String::new(),
            created: now,
            expires: now + self.wait.as_secs(),
            session_allowed: true,
            choices: Vec::new(),
            started_by: Vec::new(),
        };
        let verdict = self.approver.ask_verdict(env, &p, self.wait);
        let note = verdict.as_ref().and_then(|v| v.note.clone());
        let denied = || {
            Response::new(
                Status::Denied,
                crate::broker::denied_message("the user denied the write", note.as_deref()),
            )
        };
        let (decision, resp) = match verdict.map(|v| v.answer) {
            Some(Answer::Once) => ("allowed once", None),
            Some(Answer::Session) => {
                if let Err(e) = grant(env, self.name, self.pasture, self.project) {
                    log::warn!("cannot record the principal grant: {e:#}");
                }
                ("allowed for this session", None)
            }
            Some(Answer::Deny) | Some(Answer::DenySession) => ("denied", Some(denied())),
            None => (
                "timed out",
                Some(Response::new(
                    Status::Timeout,
                    format!("nobody answered within {} seconds", self.wait.as_secs()),
                )),
            ),
        };
        let status = resp.as_ref().map_or(Status::Done, |r| r.status);
        self.audit(env, req, decision, status);
        resp
    }

    /// Check the principal and the pasture, ask the user on a first write,
    /// and run the request in the pasture.
    pub fn handle(&self, env: &HostEnv, cfg: &KvCfg, req: &Request) -> Response {
        let Some(p) = cfg.principals.get(self.name) else {
            return self.refuse(
                env,
                req,
                format!(
                    "no principal '{}' in [kv.principals] of the user configuration",
                    self.name
                ),
            );
        };
        if !p.pastures.iter().any(|n| n == self.pasture) {
            return self.refuse(
                env,
                req,
                format!(
                    "the principal '{}' may not use the pasture '{}': add it to pastures in [kv.principals.{}]",
                    self.name, self.pasture, self.name
                ),
            );
        }
        if !OPS.contains(&req.op.as_str()) {
            return Response::new(
                Status::Invalid,
                format!("a principal can use {}", OPS.join(", ")),
            );
        }
        let write = WRITE_OPS.contains(&req.op.as_str());
        if write && !p.write {
            return self.refuse(
                env,
                req,
                format!(
                    "the principal '{}' reads and watches only (write = false)",
                    self.name
                ),
            );
        }
        let mut req = req.clone();
        req.scope = Some(super::Scope::Pasture);
        // A principal frees only its own leases.
        req.force = None;
        if write {
            if let Some(r) = self.gate(env, &req) {
                return r;
            }
        }
        let id = super::Identity {
            sandbox: self.writer(),
            harness: HARNESS.into(),
            project: self.project.to_path_buf(),
            pasture: Some(self.pasture.into()),
            parent: None,
            chain: Vec::new(),
            tree: None,
        };
        let svc = Service::new(
            env.clone(),
            id,
            cfg.clone(),
            self.approver.clone(),
            self.wait,
        )
        .with_root(self.root.clone().unwrap_or_else(|| kv_root(env)));
        svc.handle(&req, None)
    }
}

/// `paddock kv put|delete|watch|claim|release --as NAME --pasture P`.
pub fn cli(env: &HostEnv, name: &str, project: &Path, pasture: &str, req: Request) -> Result<i32> {
    let cfg = KvCfg::load(env)?;
    let wait = crate::broker::HostPolicy::load(env)
        .map(|p| p.wait)
        .unwrap_or(120);
    let call = Call {
        name,
        project,
        pasture,
        approver: Arc::new(crate::broker::HostApprover),
        wait: Duration::from_secs(wait),
        root: None,
    };
    let resp = call.handle(env, &cfg, &req);
    if resp.status == Status::Done {
        println!("{}", serde_json::to_string_pretty(&resp.result)?);
    } else {
        if !resp.result.is_null() {
            println!("{}", serde_json::to_string_pretty(&resp.result)?);
        }
        eprintln!("paddock: {}", resp.message);
    }
    Ok(resp.status.exit_code())
}
