//! paddock kv: a small key-value store for agents, kept on the host.
//!
//! Agents use it for state that must outlive one harness or one run: a task
//! for a child, a result for a parent, a tip about a tool for the next agent
//! in the project. The store is one SQLite file in paddock's state folder
//! (store.rs). Each sandbox reaches it through its own Unix socket, which the
//! paddock process of that sandbox serves. The socket is the identity: the
//! service knows which sandbox it serves, so a request never names a sandbox
//! and cannot claim to be another one.
//!
//! Scopes:
//!
//! - `self`: this sandbox (this run). Removed by `paddock gc` some days after
//!   the run ends.
//! - `project`: every sandbox of the same project folder. Each entry records
//!   the sandbox that wrote it. Readers get it marked as untrusted, because
//!   an agent with a prompt injection can write there.
//! - `parent`: the `self` scope of the sandbox that started this one, read
//!   only. Only a sandbox started by another sandbox has one.
//! - `tree`: shared by one spawn tree. The root sandbox (the one the user
//!   started) owns it; the root and every sandbox below it read and write
//!   it, and each entry records its writer. It is on in a sandbox that can
//!   use `spawn` and in every sandbox that `spawn` started. When the root
//!   ends, the scope is read-only; the data stays for review until
//!   `paddock gc` removes it with the root's `self` data.
//! - `user`: shared across all projects. Off until the user allows it: the
//!   first use asks through the approval card.
//!
//! A sandbox that another one started never gets more than its parent. It
//! writes only its own scope and `tree`: `project` is read-only for it
//! unless `[kv] child_project_write` says otherwise. For `user` it inherits
//! the session grant of the sandboxes above it. Without one, its first use
//! asks the user on a card that names the sandbox that started it.
//!
//! Key names can appear in the audit log; values never do.
//!
//! Leases (`claim`, `release`) are keys under `lease:` whose owner paddock
//! writes from the connection. Plain writes cannot change those keys.
//!
//! One extension is planned and fits this model without a new store:
//!
//! - A host API for automation: a socket in the run folder outside every
//!   sandbox, where each client is a named principal from `[kv.clients.*]`
//!   of the user configuration with the namespaces it may read and write.
//!   Its writes would record `client:<name>` as the writer, the same way
//!   the host CLI records `user`.
//!
//! `Service::ns` is the one place that maps a scope to a namespace, and
//! `Identity` is the one place that holds who calls.

pub mod escape;
pub mod host;
pub mod store;

use crate::broker::{Answer, Approver, Mode, Pending};
use crate::env::HostEnv;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use store::{Quota, Store, WriteError, Writer};

pub const PROTOCOL: u32 = 1;
/// Where the socket appears inside the sandbox.
pub const SOCKET_IN_SANDBOX: &str = "/run/paddock/kv.sock";
pub const MAX_KEY: usize = 256;
const MAX_TTL: u64 = 365 * 24 * 3600;
const MAX_WATCH: u64 = 600;
const MAX_LIST: usize = 10_000;
const MAX_CONNECTIONS: usize = 64;
/// The capability name on the approval card for the `user` scope.
pub const USER_CAPABILITY: &str = "kv-user";
/// Keys that only `claim` and `release` change.
pub const LEASE_PREFIX: &str = "lease:";
/// Keys of one mget or mset.
const MAX_BATCH: usize = 100;
/// The values of one mset hold at most this many times `max_value`.
const BATCH_VALUES: usize = 16;
pub const OPS: &[&str] = &[
    "get", "mget", "put", "mset", "delete", "list", "history", "watch", "claim", "release",
];
const WRITE_OPS: &[&str] = &["put", "mset", "delete", "claim", "release"];

pub fn store_path(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("kv").join("store.db")
}

pub fn audit_file(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("kv-audit.jsonl")
}

/// `[kv]` of `~/.config/paddock/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KvCfg {
    pub enabled: bool,
    /// The `user` scope: ask (default), allow or deny.
    pub user: Mode,
    pub max_value: usize,
    pub self_bytes: u64,
    pub project_bytes: u64,
    pub user_bytes: u64,
    pub max_keys: u64,
    /// `paddock gc` removes the `self` data of a run, and the `tree` scope
    /// of a root, this many days after it ended.
    pub keep_days: u64,
    /// Whether a sandbox that another one started can write the `project`
    /// scope. Off by default: what a child learns goes to `tree`, and the
    /// parent decides what the project keeps.
    pub child_project_write: bool,
}

impl Default for KvCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            user: Mode::Ask,
            max_value: 64 * 1024,
            self_bytes: 4 << 20,
            project_bytes: 16 << 20,
            user_bytes: 4 << 20,
            max_keys: 4096,
            keep_days: 30,
            child_project_write: false,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCfg {
    enabled: Option<bool>,
    user: Option<Mode>,
    max_value: Option<u64>,
    self_bytes: Option<u64>,
    project_bytes: Option<u64>,
    user_bytes: Option<u64>,
    max_keys: Option<u64>,
    keep_days: Option<u64>,
    child_project_write: Option<bool>,
}

impl KvCfg {
    pub fn load(env: &HostEnv) -> Result<Self> {
        let file = env.paddock_config().join("config.toml");
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Ok(Self::default());
        };
        Self::parse(&text).with_context(|| format!("[kv] in {} is invalid", file.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let v: toml::Value = toml::from_str(text)?;
        let raw: RawCfg = match v.get("kv") {
            Some(t) => t.clone().try_into()?,
            None => RawCfg::default(),
        };
        let d = Self::default();
        let range = |name: &str, v: Option<u64>, def: u64, lo: u64, hi: u64| -> Result<u64> {
            match v {
                None => Ok(def),
                Some(n) if (lo..=hi).contains(&n) => Ok(n),
                Some(_) => anyhow::bail!("{name} must be {lo} to {hi}"),
            }
        };
        Ok(Self {
            enabled: raw.enabled.unwrap_or(d.enabled),
            user: raw.user.unwrap_or(d.user),
            max_value: range("max_value", raw.max_value, d.max_value as u64, 1, 1 << 20)? as usize,
            self_bytes: range("self_bytes", raw.self_bytes, d.self_bytes, 1024, 1 << 30)?,
            project_bytes: range(
                "project_bytes",
                raw.project_bytes,
                d.project_bytes,
                1024,
                1 << 30,
            )?,
            user_bytes: range("user_bytes", raw.user_bytes, d.user_bytes, 1024, 1 << 30)?,
            max_keys: range("max_keys", raw.max_keys, d.max_keys, 1, 1_000_000)?,
            keep_days: range("keep_days", raw.keep_days, d.keep_days, 1, 3650)?,
            child_project_write: raw.child_project_write.unwrap_or(d.child_project_write),
        })
    }

    fn quota(&self, scope: Scope) -> Quota {
        Quota {
            bytes: match scope {
                Scope::Project | Scope::Tree => self.project_bytes,
                Scope::User => self.user_bytes,
                _ => self.self_bytes,
            },
            keys: self.max_keys,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    #[serde(rename = "self")]
    Own,
    Project,
    Parent,
    Tree,
    User,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Scope::Own => "self",
            Scope::Project => "project",
            Scope::Parent => "parent",
            Scope::Tree => "tree",
            Scope::User => "user",
        }
    }
    /// Entries that another sandbox can write.
    pub fn shared(self) -> bool {
        matches!(self, Scope::Project | Scope::Tree | Scope::User)
    }
}

impl std::str::FromStr for Scope {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "self" => Ok(Scope::Own),
            "project" => Ok(Scope::Project),
            "parent" => Ok(Scope::Parent),
            "tree" => Ok(Scope::Tree),
            "user" => Ok(Scope::User),
            _ => Err(format!(
                "unknown scope '{s}': use self, project, parent, tree or user"
            )),
        }
    }
}

/// The sandbox a service serves. It comes from paddock, never from a request.
#[derive(Debug, Clone)]
pub struct Identity {
    pub sandbox: String,
    pub harness: String,
    pub project: PathBuf,
    /// The sandbox that started this one (`spawn`). None: the user did.
    pub parent: Option<String>,
    /// The sandboxes above this one, the root first and the parent last.
    /// Empty for a sandbox that the user started.
    pub chain: Vec<String>,
    /// The root of the spawn tree, when this sandbox has a `tree` scope.
    pub tree: Option<String>,
}

impl Identity {
    /// Whether another sandbox started this one.
    pub fn is_child(&self) -> bool {
        self.parent.is_some()
    }
}

/// Where a sandbox sits in a spawn tree. It comes from the host: the
/// parent's paddock writes it into the job of the child.
#[derive(Debug, Clone, Default)]
pub struct Lineage {
    pub parent: Option<String>,
    /// The sandboxes above this one, the root first and the parent last.
    pub chain: Vec<String>,
    /// Whether this sandbox can start children. A root gets a `tree` scope
    /// only then.
    pub can_spawn: bool,
}

impl Lineage {
    /// The root of the tree, when this sandbox has a `tree` scope.
    pub fn tree_root(&self, me: &str) -> Option<String> {
        match self.chain.first() {
            Some(root) => Some(root.clone()),
            None if self.parent.is_some() => self.parent.clone(),
            None if self.can_spawn => Some(me.to_string()),
            None => None,
        }
    }
}

/// One request. Unknown fields are an error: a request has no field that
/// names a sandbox, and one that tries is rejected, not ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub v: u32,
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// put and delete: the version the key must have (0: must not exist).
    /// watch: the version the caller knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    /// put: seconds until the key expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// watch: seconds to wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// mget: the keys to read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<String>>,
    /// mset: the keys and values to write, all or none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<std::collections::BTreeMap<String, String>>,
    /// release: free a lease that another sandbox holds. Only a sandbox
    /// above that one in its spawn tree can.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
    /// claim: accepted for clients that send it, and ignored. The owner of a
    /// lease is always the sandbox of the connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Done,
    NotFound,
    Conflict,
    TooLarge,
    Denied,
    Disabled,
    Timeout,
    Invalid,
    Failed,
}

impl Status {
    /// The exit code of `paddock-host kv`. 3 to 8 mean the same as for the
    /// other `paddock-host` commands.
    pub fn exit_code(self) -> i32 {
        match self {
            Status::Done => 0,
            Status::NotFound => 1,
            Status::Denied => 3,
            Status::Disabled => 4,
            Status::Timeout => 5,
            Status::Failed => 7,
            Status::Invalid => 8,
            Status::Conflict => 9,
            Status::TooLarge => 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub status: Status,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub result: serde_json::Value,
}

impl Response {
    fn new(status: Status, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            result: serde_json::Value::Null,
        }
    }
    fn done(result: serde_json::Value) -> Self {
        Self {
            status: Status::Done,
            message: String::new(),
            result,
        }
    }
}

fn check_key(key: &str, what: &str, empty_ok: bool) -> Result<(), String> {
    if key.is_empty() && !empty_ok {
        return Err(format!("the {what} is empty"));
    }
    if key.len() > MAX_KEY {
        return Err(format!("the {what} is longer than {MAX_KEY} bytes"));
    }
    if key.chars().any(char::is_control) {
        return Err(format!("the {what} contains a control character"));
    }
    Ok(())
}

/// The service of one sandbox.
pub struct Service {
    env: HostEnv,
    id: Identity,
    cfg: KvCfg,
    approver: Arc<dyn Approver>,
    wait: Duration,
    path: PathBuf,
    user_granted: AtomicBool,
    /// One user-scope question at a time; the others wait for its answer.
    asking: Mutex<()>,
    connections: AtomicUsize,
}

impl Service {
    pub fn new(
        env: HostEnv,
        id: Identity,
        cfg: KvCfg,
        approver: Arc<dyn Approver>,
        wait: Duration,
    ) -> Self {
        let path = store_path(&env);
        Self {
            env,
            id,
            cfg,
            approver,
            wait,
            path,
            user_granted: AtomicBool::new(false),
            asking: Mutex::new(()),
            connections: AtomicUsize::new(0),
        }
    }

    /// Use another store file. Tests use it.
    pub fn with_store(mut self, path: PathBuf) -> Self {
        self.path = path;
        self
    }

    pub fn identity(&self) -> &Identity {
        &self.id
    }

    fn ns(&self, scope: Scope) -> Option<String> {
        Some(match scope {
            Scope::Own => store::ns_self(&self.id.sandbox),
            Scope::Project => store::ns_project(&self.id.project),
            Scope::Parent => store::ns_self(self.id.parent.as_deref()?),
            Scope::Tree => store::ns_tree(self.id.tree.as_deref()?),
            Scope::User => store::NS_USER.to_string(),
        })
    }

    fn audit(&self, req: &Request, scope: Scope, decision: &str, resp: &Response) {
        let line = serde_json::json!({
            "time": crate::state::now(),
            "sandbox": self.id.sandbox,
            "harness": self.id.harness,
            "project": self.id.project,
            "op": req.op,
            "scope": scope.name(),
            "key": req.key.as_deref().or(req.prefix.as_deref()).map(|k| k.chars().take(MAX_KEY).collect::<String>()),
            // The size only: values never reach the log.
            "bytes": req.value.as_ref().map(|v| v.len()),
            "decision": decision,
            "status": resp.status,
        });
        let f = audit_file(&self.env);
        match crate::privfs::append(&f) {
            Ok(mut file) => {
                let _ = writeln!(file, "{line}");
            }
            Err(e) => log::warn!("cannot open the kv audit log {}: {e}", f.display()),
        }
    }

    /// The decision for the `user` scope: None when allowed.
    fn user_gate(&self, req: &Request) -> Option<Response> {
        match self.cfg.user {
            Mode::Deny => {
                return Some(Response::new(
                    Status::Disabled,
                    "the user scope is set to deny in [kv] of the user configuration",
                ))
            }
            Mode::Allow => return None,
            Mode::Ask => {}
        }
        let _one = self.asking.lock().unwrap();
        if self.user_granted.load(Ordering::SeqCst) {
            return None;
        }
        // A child inherits the session grant of the sandboxes above it. The
        // chain comes from the host, never from the request.
        if self.inherited_user_grant() {
            self.user_granted.store(true, Ordering::SeqCst);
            return None;
        }
        // A sandbox that another one started never asks on its own: one
        // prompt injection must not turn into many questions. The root
        // sandbox asks (its children then inherit the grant), or the user
        // allows the scope in the configuration.
        if self.id.is_child() {
            return Some(Response::new(
                Status::Denied,
                "a sandbox started by spawn cannot ask for the user scope; use it in the root sandbox first, or set [kv] user = \"allow\" in the user configuration",
            ));
        }
        let started = crate::spawn::chain_text(&self.id.chain);
        let p = Pending {
            id: request_id(),
            sandbox: self.id.sandbox.clone(),
            harness: self.id.harness.clone(),
            project: self.id.project.clone(),
            capability: USER_CAPABILITY.into(),
            summary: "use the user-wide paddock kv scope".into(),
            detail: format!(
                "{}The sandbox wants to {} the user scope of paddock kv. Sandboxes of all your projects share this scope, so what one agent writes there, agents in other projects read.",
                started.map(|t| format!("{t} ")).unwrap_or_default(),
                if WRITE_OPS.contains(&req.op.as_str()) { "write to" } else { "read" }
            ),
            reason: String::new(),
            created: crate::state::now(),
            expires: crate::state::now() + self.wait.as_secs(),
            session_allowed: true,
            started_by: self.id.chain.clone(),
        };
        let (decision, resp) = match self.approver.ask(&self.env, &p, self.wait) {
            Some(Answer::Once) => ("allowed once", None),
            Some(Answer::Session) => {
                self.user_granted.store(true, Ordering::SeqCst);
                // The children of this sandbox inherit it.
                if let Err(e) =
                    Store::open(&self.path).and_then(|s| s.grant(&self.id.sandbox, USER_CAPABILITY))
                {
                    log::warn!("cannot record the user scope grant: {e:#}");
                }
                ("allowed for this session", None)
            }
            Some(Answer::Deny) => (
                "denied",
                Some(Response::new(
                    Status::Denied,
                    "the user denied the user scope",
                )),
            ),
            None => (
                "timed out",
                Some(Response::new(
                    Status::Timeout,
                    format!("nobody answered within {} seconds", self.wait.as_secs()),
                )),
            ),
        };
        let shown = resp
            .clone()
            .unwrap_or_else(|| Response::done(serde_json::Value::Null));
        self.audit(req, Scope::User, decision, &shown);
        resp
    }

    /// Whether a sandbox above this one has the user scope for its session.
    fn inherited_user_grant(&self) -> bool {
        if self.id.chain.is_empty() {
            return false;
        }
        let Ok(st) = Store::open(&self.path) else {
            return false;
        };
        self.id
            .chain
            .iter()
            .any(|a| st.granted(a, USER_CAPABILITY).unwrap_or(false))
    }

    /// Whether this sandbox may write the project scope.
    fn project_writable(&self) -> bool {
        !self.id.is_child() || self.cfg.child_project_write
    }

    fn info(&self) -> serde_json::Value {
        serde_json::json!({
            "protocol": PROTOCOL,
            "sandbox": self.id.sandbox,
            "harness": self.id.harness,
            "project": self.id.project,
            "parent": self.id.parent,
            "started_by": self.id.chain,
            "tree_root": self.id.tree,
            "scopes": {
                "self": "read-write; this sandbox only",
                "project": if self.project_writable() { "read-write; shared with every sandbox of this project; untrusted" } else { "read-only for a sandbox that another one started ([kv] child_project_write); shared with every sandbox of this project; untrusted" },
                "parent": if self.id.parent.is_some() { "read-only; the self scope of the sandbox that started this one" } else { "none: no sandbox started this one" },
                "tree": match &self.id.tree {
                    Some(r) if r == &self.id.sandbox => "read-write; shared with every sandbox that this one starts, and their children; untrusted".to_string(),
                    Some(r) => format!("read-write; shared with sandbox {r}, which started this spawn tree, and every sandbox in it; untrusted"),
                    None => "none: only a sandbox that can start other agents (spawn), and the sandboxes it starts, have one".to_string(),
                },
                "user": match self.cfg.user {
                    Mode::Deny => "off".to_string(),
                    Mode::Allow => "read-write; shared with every project; untrusted".to_string(),
                    Mode::Ask if self.user_granted.load(Ordering::SeqCst) => "read-write for this session; shared with every project; untrusted".to_string(),
                    Mode::Ask => "asks the user at first use; shared with every project; untrusted".to_string(),
                },
            },
            "limits": {
                "max_key_bytes": MAX_KEY,
                "max_value_bytes": self.cfg.max_value,
                "self_bytes": self.cfg.self_bytes,
                "project_bytes": self.cfg.project_bytes,
                "tree_bytes": self.cfg.project_bytes,
                "user_bytes": self.cfg.user_bytes,
                "max_keys_per_scope": self.cfg.max_keys,
                "max_ttl_seconds": MAX_TTL,
                "max_watch_seconds": MAX_WATCH,
            },
        })
    }

    /// Handle one request. `conn` lets a watch stop when the client goes away.
    pub fn handle(&self, req: &Request, conn: Option<&UnixStream>) -> Response {
        let scope = req.scope.unwrap_or_default();
        let resp = self.handle_inner(req, scope, conn);
        let write = WRITE_OPS.contains(&req.op.as_str());
        let failed = matches!(
            resp.status,
            Status::Denied | Status::Disabled | Status::Invalid | Status::Failed
        );
        // The user scope gate logs its own decision.
        let gated = scope == Scope::User && resp.status == Status::Denied;
        if (write || failed) && !gated {
            self.audit(req, scope, "", &resp);
        }
        log::debug!(
            "sandbox {}: kv {} {} {:?}: {:?}",
            self.id.sandbox,
            req.op,
            scope.name(),
            req.key.as_deref().unwrap_or(""),
            resp.status
        );
        if resp.status == Status::Failed {
            log::warn!(
                "sandbox {}: kv {} failed: {}",
                self.id.sandbox,
                req.op,
                resp.message
            );
        }
        resp
    }

    fn handle_inner(&self, req: &Request, scope: Scope, conn: Option<&UnixStream>) -> Response {
        let invalid = |m: String| Response::new(Status::Invalid, m);
        if req.v != PROTOCOL {
            return invalid(format!(
                "protocol version {} is not supported; this paddock speaks {PROTOCOL}",
                req.v
            ));
        }
        if !self.cfg.enabled {
            return Response::new(
                Status::Disabled,
                "paddock kv is off in [kv] of the user configuration",
            );
        }
        if req.op == "info" {
            return Response::done(self.info());
        }
        if !OPS.contains(&req.op.as_str()) {
            return invalid(format!(
                "unknown op '{}': use info, {}",
                req.op,
                OPS.join(", ")
            ));
        }
        let write = WRITE_OPS.contains(&req.op.as_str());
        let Some(ns) = self.ns(scope) else {
            return Response::new(
                Status::Disabled,
                if scope == Scope::Tree {
                    "this sandbox has no tree scope: only a sandbox that can start other agents (paddock-host spawn), and the sandboxes it starts, have one"
                } else {
                    "this sandbox has no parent scope: only a sandbox that another sandbox started has one"
                },
            );
        };
        if write && scope == Scope::Parent {
            return Response::new(Status::Denied, "the parent scope is read-only");
        }
        if write && scope == Scope::Project && !self.project_writable() {
            return Response::new(
                Status::Denied,
                "a sandbox that another sandbox started cannot write the project scope: write to the tree scope, and the sandbox that started you decides what the project keeps ([kv] child_project_write in the user configuration)",
            );
        }
        // Check the whole request before the user is asked anything.
        if let Err(e) = self.check(req) {
            return e;
        }
        if scope == Scope::User {
            if let Some(r) = self.user_gate(req) {
                return r;
            }
        }
        let mut st = match Store::open(&self.path) {
            Ok(s) => s,
            Err(e) => return Response::new(Status::Failed, format!("{e:#}")),
        };
        // The tree ends with its root. Its data stays for review, read-only.
        if write && scope == Scope::Tree {
            let root = self.id.tree.as_deref().unwrap_or_default();
            match st.ended(root) {
                Ok(Some(false)) => {}
                Ok(_) => {
                    return Response::new(
                        Status::Denied,
                        format!(
                            "the spawn tree of sandbox {root} ended; its tree scope is read-only"
                        ),
                    )
                }
                Err(e) => return Response::new(Status::Failed, format!("{e:#}")),
            }
        }
        let r = self.run(&mut st, req, scope, &ns, conn);
        r.unwrap_or_else(|e| Response::new(Status::Failed, format!("{e:#}")))
    }

    /// Checks that need no store: keys, sizes, times and counts.
    fn check(&self, req: &Request) -> Result<(), Response> {
        let invalid = |m: String| Err(Response::new(Status::Invalid, m));
        let too_large = |m: String| Err(Response::new(Status::TooLarge, m));
        let op = req.op.as_str();
        let needs_key = matches!(
            op,
            "get" | "put" | "delete" | "history" | "claim" | "release"
        ) || (op == "watch" && req.prefix.is_none());
        if needs_key {
            match &req.key {
                None => return invalid(format!("{op} needs a key")),
                Some(k) => check_key(k, "key", false).or_else(invalid)?,
            }
        }
        if op == "watch" && req.key.is_some() && req.prefix.is_some() {
            return invalid("watch takes a key or a prefix, not both".into());
        }
        if let Some(p) = &req.prefix {
            check_key(p, "prefix", true).or_else(invalid)?;
        }
        // Lease keys hold an owner that paddock writes. A plain write could
        // forge one, so only claim and release change them.
        let leased = |k: &str| k.starts_with(LEASE_PREFIX);
        if matches!(op, "put" | "delete") && req.key.as_deref().is_some_and(leased) {
            return invalid(format!(
                "keys that start with {LEASE_PREFIX} change only through claim and release"
            ));
        }
        if req.ttl.is_some_and(|t| t == 0 || t > MAX_TTL) {
            return invalid(format!("ttl must be 1 to {MAX_TTL} seconds"));
        }
        let value_ok = |v: &str| -> Result<(), Response> {
            if v.len() > self.cfg.max_value {
                return too_large(format!(
                    "a value has {} bytes; the limit is {}",
                    v.len(),
                    self.cfg.max_value
                ));
            }
            Ok(())
        };
        match op {
            "put" => match &req.value {
                None => return invalid("put needs a value".into()),
                Some(v) => value_ok(v)?,
            },
            "mget" => {
                let keys = req.keys.as_deref().unwrap_or_default();
                if keys.is_empty() || keys.len() > MAX_BATCH {
                    return invalid(format!("mget needs 1 to {MAX_BATCH} keys"));
                }
                for k in keys {
                    check_key(k, "key", false).or_else(invalid)?;
                }
            }
            "mset" => {
                let Some(entries) = req.entries.as_ref().filter(|e| !e.is_empty()) else {
                    return invalid("mset needs entries".into());
                };
                if entries.len() > MAX_BATCH {
                    return invalid(format!("mset takes at most {MAX_BATCH} entries"));
                }
                let mut total = 0;
                for (k, v) in entries {
                    check_key(k, "key", false).or_else(invalid)?;
                    if leased(k) {
                        return invalid(format!(
                            "keys that start with {LEASE_PREFIX} change only through claim and release"
                        ));
                    }
                    value_ok(v)?;
                    total += v.len();
                }
                if total > self.cfg.max_value * BATCH_VALUES {
                    return too_large(format!(
                        "the values have {total} bytes together; the limit is {}",
                        self.cfg.max_value * BATCH_VALUES
                    ));
                }
            }
            "claim" if req.ttl.is_none() => {
                return invalid("claim needs a ttl: a lease must expire".into())
            }
            "watch" => {
                let t = req.timeout.unwrap_or(60);
                if t == 0 || t > MAX_WATCH {
                    return invalid(format!("timeout must be 1 to {MAX_WATCH} seconds"));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn writer(&self) -> Writer<'_> {
        Writer {
            sandbox: &self.id.sandbox,
            harness: &self.id.harness,
        }
    }

    fn run(
        &self,
        st: &mut Store,
        req: &Request,
        scope: Scope,
        ns: &str,
        conn: Option<&UnixStream>,
    ) -> Result<Response> {
        let now = crate::state::now();
        let key = req.key.as_deref().unwrap_or("");
        let shared = scope.shared();
        let mark = |mut v: serde_json::Value| {
            if shared {
                v["untrusted"] = true.into();
            }
            v
        };
        let quota = self.cfg.quota(scope);
        Ok(match req.op.as_str() {
            "get" => match st.get(ns, key, now)? {
                Some(e) => Response::done(mark(entry_json(&e))),
                None => Response::new(Status::NotFound, format!("no key {key:?}")),
            },
            "mget" => {
                let mut out = Vec::new();
                for k in req.keys.as_deref().unwrap_or_default() {
                    out.push(match st.get(ns, k, now)? {
                        Some(e) => entry_json(&e),
                        None => serde_json::Value::Null,
                    });
                }
                Response::done(mark(serde_json::json!({ "entries": out })))
            }
            "put" => {
                let expires = req.ttl.map(|t| now + t);
                let value = req.value.as_deref().unwrap_or("");
                match st.put(
                    ns,
                    key,
                    value,
                    req.version,
                    expires,
                    &self.writer(),
                    quota,
                    now,
                ) {
                    Ok(v) => Response::done(serde_json::json!({"version": v, "expires": expires})),
                    Err(e) => write_error(e)?,
                }
            }
            "mset" => {
                let expires = req.ttl.map(|t| now + t);
                let entries = req.entries.clone().unwrap_or_default();
                let puts: Vec<store::Put> = entries
                    .iter()
                    .map(|(k, v)| store::Put {
                        key: k,
                        value: v,
                        if_version: None,
                        expires,
                    })
                    .collect();
                match st.put_many(ns, &puts, &self.writer(), quota, now) {
                    Ok(vs) => {
                        let versions: std::collections::BTreeMap<&String, u64> =
                            entries.keys().zip(vs).collect();
                        Response::done(serde_json::json!({ "versions": versions }))
                    }
                    Err(e) => write_error(e)?,
                }
            }
            "delete" => match st.delete(ns, key, req.version, &self.writer(), quota, now) {
                Ok(()) => Response::done(serde_json::json!({"deleted": true})),
                Err(e) => write_error(e)?,
            },
            "list" => {
                let limit = req.limit.unwrap_or(1000).clamp(1, MAX_LIST);
                let prefix = req.prefix.as_deref().unwrap_or("");
                let (items, truncated) = st.list(ns, prefix, limit, now)?;
                Response::done(mark(
                    serde_json::json!({ "entries": items, "truncated": truncated }),
                ))
            }
            "history" => {
                let limit = req
                    .limit
                    .unwrap_or(store::HISTORY_PER_KEY as usize)
                    .clamp(1, store::HISTORY_PER_KEY as usize);
                let h = st.history(ns, key, limit)?;
                Response::done(mark(serde_json::json!({ "history": h })))
            }
            "claim" => self.claim(st, ns, key, req.ttl.unwrap_or(60), quota, now)?,
            "release" => self.release(st, ns, key, req.force == Some(true), quota, now)?,
            "watch" => self.watch(st, ns, req, conn, &mark)?,
            _ => unreachable!(),
        })
    }

    /// Take the lease `lease:<name>`, or renew it when this sandbox holds it.
    fn claim(
        &self,
        st: &mut Store,
        ns: &str,
        name: &str,
        ttl: u64,
        quota: Quota,
        now: u64,
    ) -> Result<Response> {
        let key = format!("{LEASE_PREFIX}{name}");
        let cur = st.get(ns, &key, now)?;
        if let Some(e) = &cur {
            if e.writer != self.id.sandbox {
                return Ok(Response {
                    status: Status::Conflict,
                    message: format!("sandbox {} holds the lease", e.writer),
                    result: serde_json::json!({ "lease": key, "owner": e.writer, "expires": e.expires }),
                });
            }
        }
        let expires = now + ttl;
        let value = serde_json::json!({
            "owner": self.id.sandbox,
            "harness": self.id.harness,
            "expires": expires,
        })
        .to_string();
        let want = Some(cur.as_ref().map_or(0, |e| e.version));
        match st.put(
            ns,
            &key,
            &value,
            want,
            Some(expires),
            &self.writer(),
            quota,
            now,
        ) {
            Ok(v) => Ok(Response::done(serde_json::json!({
                "lease": key, "owner": self.id.sandbox, "expires": expires,
                "version": v, "renewed": cur.is_some(),
            }))),
            // Another sandbox took it between the read and the write.
            Err(WriteError::Conflict { .. }) => Ok(Response::new(
                Status::Conflict,
                "another sandbox took the lease at the same moment",
            )),
            Err(e) => write_error(e),
        }
    }

    /// Free the lease `lease:<name>`. Only its owner can, or with `force` a
    /// sandbox above the owner in its spawn tree. A sibling cannot.
    fn release(
        &self,
        st: &mut Store,
        ns: &str,
        name: &str,
        force: bool,
        quota: Quota,
        now: u64,
    ) -> Result<Response> {
        let key = format!("{LEASE_PREFIX}{name}");
        let Some(e) = st.get(ns, &key, now)? else {
            return Ok(Response::new(Status::NotFound, format!("no lease {key:?}")));
        };
        if e.writer != self.id.sandbox {
            let above = st.ancestors(&e.writer)?;
            if !force || !above.contains(&self.id.sandbox) {
                return Ok(Response {
                    status: Status::Denied,
                    message: format!(
                        "sandbox {} holds the lease; only it, a sandbox above it with --force, or the user on the host can release it",
                        e.writer
                    ),
                    result: serde_json::json!({ "lease": key, "owner": e.writer, "expires": e.expires }),
                });
            }
        }
        match st.delete(ns, &key, Some(e.version), &self.writer(), quota, now) {
            Ok(()) => Ok(Response::done(
                serde_json::json!({ "lease": key, "released": true, "owner": e.writer }),
            )),
            Err(WriteError::Conflict { .. }) | Err(WriteError::Missing) => Ok(Response::new(
                Status::Conflict,
                "the lease changed at the same moment",
            )),
            Err(e) => write_error(e),
        }
    }

    /// Wait for a change. With a key: until its version differs from the
    /// known one (without one, from the version now). With a prefix: until a
    /// key with that prefix is written or deleted after the known version
    /// (without one, after now). Other sandboxes write through other
    /// processes, so this polls the store.
    fn watch(
        &self,
        st: &Store,
        ns: &str,
        req: &Request,
        conn: Option<&UnixStream>,
        mark: &dyn Fn(serde_json::Value) -> serde_json::Value,
    ) -> Result<Response> {
        let timeout = req.timeout.unwrap_or(60);
        let end = Instant::now() + Duration::from_secs(timeout);
        let prefix = req.prefix.as_deref();
        let key = req.key.as_deref().unwrap_or("");
        let known = match (req.version, prefix) {
            (Some(v), _) => v,
            (None, Some(_)) => st.seq()?,
            (None, None) => st.version(ns, key, crate::state::now())?,
        };
        loop {
            let now = crate::state::now();
            if let Some(p) = prefix {
                let changes = st.changes(ns, p, known, 1000)?;
                if let Some(last) = changes.last() {
                    let cursor = last.version;
                    return Ok(Response::done(mark(
                        serde_json::json!({ "changes": changes, "cursor": cursor }),
                    )));
                }
            } else if st.version(ns, key, now)? != known {
                return Ok(Response::done(match st.get(ns, key, now)? {
                    Some(e) => mark(entry_json(&e)),
                    None => serde_json::json!({ "key": key, "version": 0, "deleted": true }),
                }));
            }
            if Instant::now() >= end {
                return Ok(Response::new(
                    Status::Timeout,
                    format!("nothing changed in {timeout} seconds"),
                ));
            }
            if conn.is_some_and(peer_gone) {
                return Ok(Response::new(Status::Timeout, "the client went away"));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn serve_conn(&self, conn: UnixStream) -> Result<()> {
        conn.set_read_timeout(Some(Duration::from_secs(30)))?;
        // JSON can escape one byte as six.
        let max = (self.cfg.max_value * BATCH_VALUES * 6 + 64 * 1024) as u64;
        let mut reader = BufReader::new(conn.try_clone()?.take(max));
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let resp = if !line.ends_with('\n') && line.len() as u64 >= max {
            Response::new(Status::TooLarge, "the request is too large")
        } else {
            match serde_json::from_str::<Request>(line.trim()) {
                Ok(req) => self.handle(&req, Some(&conn)),
                Err(e) => {
                    let r = Response::new(Status::Invalid, format!("bad request: {e}"));
                    let shown = Request {
                        op: "unparsed".into(),
                        ..Default::default()
                    };
                    self.audit(&shown, Scope::Own, "", &r);
                    r
                }
            }
        };
        let mut w = conn;
        writeln!(w, "{}", serde_json::to_string(&resp)?)?;
        Ok(())
    }
}

fn entry_json(e: &store::Entry) -> serde_json::Value {
    serde_json::json!({
        "key": e.key, "value": e.value, "version": e.version,
        "writer": e.writer, "writer_harness": e.writer_harness,
        "updated": e.updated, "expires": e.expires,
    })
}

fn write_error(e: WriteError) -> Result<Response> {
    Ok(match e {
        WriteError::Conflict { key, current } => Response {
            status: Status::Conflict,
            message: format!("{key:?} has version {current}"),
            result: serde_json::json!({ "key": key, "version": current }),
        },
        WriteError::Quota(m) => Response::new(Status::TooLarge, m),
        WriteError::Missing => Response::new(Status::NotFound, "no such key"),
        WriteError::Db(e) => return Err(e),
    })
}

/// True when the other end closed its side.
fn peer_gone(conn: &UnixStream) -> bool {
    let mut p = libc::pollfd {
        fd: conn.as_raw_fd(),
        events: libc::POLLIN | libc::POLLRDHUP,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    n > 0 && p.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0
}

fn request_id() -> String {
    use sha2::{Digest, Sha256};
    let seed = format!("{:?}-{}-kv", Instant::now(), std::process::id());
    let h = crate::manifest::hex(&Sha256::digest(seed.as_bytes()));
    format!("r-{}-{}", crate::state::now(), &h[..8])
}

/// A running service. `finish` records the end of the run in the store.
pub struct Served {
    path: PathBuf,
    sandbox: String,
}

impl Served {
    pub fn finish(self) {
        if let Err(e) =
            Store::open(&self.path).and_then(|s| s.finish(&self.sandbox, crate::state::now()))
        {
            log::debug!(
                "cannot record the end of {} in the kv store: {e:#}",
                self.sandbox
            );
        }
    }
}

/// Serve requests on `socket` until the process exits.
pub fn serve(socket: &Path, svc: Service) -> Result<Served> {
    let st = Store::open(&svc.path)?;
    st.register(
        &svc.id.sandbox,
        &svc.id.harness,
        &svc.id.project,
        svc.id.parent.as_deref(),
        crate::state::now(),
    )?;
    drop(st);
    let _ = std::fs::remove_file(socket);
    if let Some(d) = socket.parent() {
        crate::privfs::dir(d)?;
    }
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("cannot listen on {}", socket.display()))?;
    let served = Served {
        path: svc.path.clone(),
        sandbox: svc.id.sandbox.clone(),
    };
    let svc = Arc::new(svc);
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            if svc.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                svc.connections.fetch_sub(1, Ordering::SeqCst);
                let mut c = conn;
                let r = Response::new(Status::Failed, "too many open kv connections");
                let _ = writeln!(c, "{}", serde_json::to_string(&r).unwrap_or_default());
                continue;
            }
            let s = svc.clone();
            std::thread::spawn(move || {
                if let Err(e) = s.serve_conn(conn) {
                    log::debug!("kv connection failed: {e:#}");
                }
                s.connections.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    Ok(served)
}

/// At sandbox start: serve the store on a socket in the run folder and
/// record it in the spec, for the backend. A problem is a notice: the
/// sandbox starts without the store.
/// `project` names the project scope: the project folder, or for a paddock
/// worktree the repository it belongs to, so that all its worktrees share it.
pub fn start(
    env: &HostEnv,
    spec: &mut crate::spec::SandboxSpec,
    project: PathBuf,
    approver: Arc<dyn Approver>,
    lineage: Lineage,
) -> Option<Served> {
    let cfg = match KvCfg::load(env) {
        Ok(c) => c,
        Err(e) => {
            spec.notices.push(format!("paddock kv is off: {e:#}"));
            return None;
        }
    };
    if !cfg.enabled {
        return None;
    }
    let wait = crate::broker::HostPolicy::load(env)
        .map(|p| p.wait)
        .unwrap_or(120);
    let id = Identity {
        tree: lineage.tree_root(&spec.id),
        sandbox: spec.id.clone(),
        harness: spec.harness.clone(),
        project,
        parent: lineage.parent,
        chain: lineage.chain,
    };
    let svc = Service::new(env.clone(), id, cfg, approver, Duration::from_secs(wait));
    let sock = env.paddock_run().join(&spec.id).join("kv.sock");
    match serve(&sock, svc) {
        Ok(s) => {
            spec.kv_socket = Some(sock);
            Some(s)
        }
        Err(e) => {
            spec.notices.push(format!("paddock kv is off: {e:#}"));
            None
        }
    }
}

/// What `paddock gc` can remove from the store.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct GcPlan {
    /// Finished runs whose `self` data, or whose `tree` scope as a root, is
    /// older than `keep_days`.
    pub stale: Vec<store::Stale>,
    pub expired_entries: u64,
    pub expired_bytes: u64,
}

pub fn gc_plan(env: &HostEnv, running: &[String], now: u64) -> Result<GcPlan> {
    let path = store_path(env);
    if !path.exists() {
        return Ok(GcPlan::default());
    }
    let cfg = KvCfg::load(env)?;
    let st = Store::open(&path)?;
    let cutoff = now.saturating_sub(cfg.keep_days * 24 * 3600);
    let (n, b) = st.expired(now)?;
    Ok(GcPlan {
        stale: st.stale(cutoff, running)?,
        expired_entries: n,
        expired_bytes: b,
    })
}

/// The items of `paddock gc`: one for the `self` data of finished runs and
/// expired keys. It is paddock's own data, so `--yes` alone removes it.
pub fn gc_items(env: &HostEnv, now: u64) -> Result<Vec<crate::storage::Item>> {
    let running: Vec<String> = crate::state::Running::list(env)
        .into_iter()
        .map(|r| r.id)
        .collect();
    let plan = gc_plan(env, &running, now)?;
    if plan.stale.is_empty() && plan.expired_entries == 0 {
        return Ok(Vec::new());
    }
    let days = KvCfg::load(env)?.keep_days;
    let entries: u64 = plan.stale.iter().map(|s| s.entries).sum::<u64>() + plan.expired_entries;
    Ok(vec![crate::storage::Item {
        what: format!(
            "paddock kv: {entries} keys of {} finished runs and expired keys",
            plan.stale.len()
        ),
        tier: crate::storage::Tier::Auto,
        bytes: plan.stale.iter().map(|s| s.bytes).sum::<u64>() + plan.expired_bytes,
        reason: format!(
            "the self scope of runs, and the tree scope of spawn trees, that ended more than {days} days ago ([kv] keep_days), and keys whose time to live passed"
        ),
        warning: None,
        needs: vec![],
        action: crate::storage::Action::Kv {
            sandboxes: plan.stale.iter().map(|s| s.sandbox.clone()).collect(),
        },
    }])
}

/// Remove what the plan names. Sandboxes that run now are skipped.
pub fn gc_apply(env: &HostEnv, sandboxes: &[String], now: u64) -> Result<u64> {
    let running: Vec<String> = crate::state::Running::list(env)
        .into_iter()
        .map(|r| r.id)
        .collect();
    let cfg = KvCfg::load(env)?;
    let mut st = Store::open(&store_path(env))?;
    let ids: Vec<String> = sandboxes
        .iter()
        .filter(|s| !running.contains(s))
        .cloned()
        .collect();
    let mut n = st.remove_sandboxes(&ids)?;
    n += st.purge_expired(now)?;
    st.forget_ended(now.saturating_sub(cfg.keep_days * 24 * 3600))?;
    Ok(n)
}

/// The lines of `paddock doctor` about paddock kv.
pub fn doctor_lines(env: &HostEnv) -> Vec<String> {
    let cfg = match KvCfg::load(env) {
        Ok(c) => c,
        Err(e) => return vec![format!("  error    {e:#}")],
    };
    if !cfg.enabled {
        return vec!["  off      paddock kv: [kv] enabled = false".into()];
    }
    let path = store_path(env);
    let user = match cfg.user {
        Mode::Ask => "asks at first use",
        Mode::Allow => "allowed",
        Mode::Deny => "off",
    };
    let state = if path.exists() {
        match Store::open(&path) {
            Ok(_) => format!("store {}", path.display()),
            Err(e) => return vec![format!("  error    paddock kv: {e:#}")],
        }
    } else {
        "the store is created at the first start".to_string()
    };
    vec![format!(
        "  on       paddock kv: {state}; user scope {user}; self data kept {} days after a run",
        cfg.keep_days
    )]
}

#[cfg(test)]
mod tests;
