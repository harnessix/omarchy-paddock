//! paddock kv: a small key-value store for agents, kept on the host.
//!
//! Agents use it for state that must outlive one harness or one run: a task
//! for a child, a result for a parent, a tip about a tool for the next agent
//! in the project. The store is one SQLite file per pasture, run and
//! spawn tree, and one for the `user` scope (shard.rs, store.rs). Each sandbox reaches it through its own Unix socket, which the
//! paddock process of that sandbox serves. The socket is the identity: the
//! service knows which sandbox it serves, so a request never names a sandbox
//! and cannot claim to be another one.
//!
//! Scopes:
//!
//! - `self`: this sandbox (this run). Removed by `paddock gc` some days after
//!   the run ends.
//! - `pasture`: the sandboxes of one pasture (`--pasture NAME`, see
//!   pasture.rs). A sandbox started without one is a pasture of its own,
//!   so by default no two sandboxes share it. `project` is another name
//!   for it. Each entry records the sandbox that wrote it. Readers get it
//!   marked as untrusted, because an agent with a prompt injection can
//!   write there.
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
//! has no `pasture` scope: what it shares goes to `tree`, and the sandbox
//! that started it decides what the pasture keeps. For `user` it inherits
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
pub mod shard;
pub mod store;

use crate::broker::{Answer, Approver, Mode, Pending};
use crate::env::HostEnv;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use shard::Ns;
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

/// The folder of the kv files (shard.rs).
pub fn kv_root(env: &HostEnv) -> PathBuf {
    shard::root(env)
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
    /// Older versions let spawned sandboxes write the project scope with
    /// it. Spawned sandboxes have no pasture scope now; it is accepted and
    /// has no effect, so that old configurations still load.
    #[allow(dead_code)]
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
        })
    }

    fn quota(&self, scope: Scope) -> Quota {
        Quota {
            bytes: match scope {
                Scope::Pasture | Scope::Tree => self.project_bytes,
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
    /// `project` is the name of older versions; it means the same.
    #[serde(alias = "project")]
    Pasture,
    Parent,
    Tree,
    User,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Scope::Own => "self",
            Scope::Pasture => "pasture",
            Scope::Parent => "parent",
            Scope::Tree => "tree",
            Scope::User => "user",
        }
    }
    /// Entries that another sandbox can write.
    pub fn shared(self) -> bool {
        matches!(self, Scope::Pasture | Scope::Tree | Scope::User)
    }
}

impl std::str::FromStr for Scope {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "self" => Ok(Scope::Own),
            "pasture" | "project" => Ok(Scope::Pasture),
            "parent" => Ok(Scope::Parent),
            "tree" => Ok(Scope::Tree),
            "user" => Ok(Scope::User),
            _ => Err(format!(
                "unknown scope '{s}': use self, pasture, parent, tree or user"
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
    /// The pasture this sandbox joined (`--pasture NAME`). None: it is a
    /// pasture of its own.
    pub pasture: Option<String>,
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
    /// The longest wait of a watch passed and nothing changed yet. The
    /// caller runs the same watch again (see `crate::waiting`).
    Waiting,
}

impl Status {
    /// The exit code of `paddock-host kv`. 3 to 8 mean the same as for the
    /// other `paddock-host` commands, and 75 is "still waiting" everywhere.
    pub fn exit_code(self) -> i32 {
        match self {
            Status::Done => 0,
            Status::NotFound => 1,
            Status::Denied => 3,
            Status::Disabled => 4,
            Status::Timeout => 5,
            Status::Waiting => crate::waiting::EXIT_STILL_WAITING,
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
    /// The folder of the kv files (shard.rs).
    root: PathBuf,
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
        let root = kv_root(&env);
        Self {
            env,
            id,
            cfg,
            approver,
            wait,
            root,
            user_granted: AtomicBool::new(false),
            asking: Mutex::new(()),
            connections: AtomicUsize::new(0),
        }
    }

    /// Use another kv folder. Tests use it.
    pub fn with_root(mut self, root: PathBuf) -> Self {
        self.root = root;
        self
    }

    fn registry(&self) -> Result<Store> {
        Store::open(&shard::registry(&self.root))
    }

    pub fn identity(&self) -> &Identity {
        &self.id
    }

    fn ns(&self, scope: Scope) -> Option<Ns> {
        Some(match scope {
            Scope::Own => Ns::Own(self.id.sandbox.clone()),
            // A spawned sandbox has no pasture: it shares through `tree`.
            Scope::Pasture if self.id.is_child() => return None,
            Scope::Pasture => pasture_ns(&self.id),
            Scope::Parent => Ns::Own(self.id.parent.clone()?),
            Scope::Tree => Ns::Tree(self.id.tree.clone()?),
            Scope::User => Ns::User,
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
            choices: Vec::new(),
            started_by: self.id.chain.clone(),
        };
        let (decision, resp) = match self.approver.ask(&self.env, &p, self.wait) {
            Some(Answer::Once) => ("allowed once", None),
            Some(Answer::Session) => {
                self.user_granted.store(true, Ordering::SeqCst);
                // The children of this sandbox inherit it.
                if let Err(e) = self
                    .registry()
                    .and_then(|s| s.grant(&self.id.sandbox, USER_CAPABILITY))
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
        let Ok(st) = self.registry() else {
            return false;
        };
        self.id
            .chain
            .iter()
            .any(|a| st.granted(a, USER_CAPABILITY).unwrap_or(false))
    }

    fn info(&self) -> serde_json::Value {
        serde_json::json!({
            "protocol": PROTOCOL,
            "sandbox": self.id.sandbox,
            "harness": self.id.harness,
            "project": self.id.project,
            "pasture": self.id.pasture,
            "parent": self.id.parent,
            "started_by": self.id.chain,
            "tree_root": self.id.tree,
            "scopes": {
                "self": "read-write; this sandbox only",
                "pasture": match (&self.id.pasture, self.id.is_child()) {
                    (_, true) => "none: a sandbox that another one started shares through the tree scope".to_string(),
                    (Some(n), false) => format!("read-write; shared with the members of the pasture '{n}' of this project; untrusted; 'project' is another name for it"),
                    (None, false) => "read-write; this sandbox only, because it was started without --pasture; 'project' is another name for it".to_string(),
                },
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
                "pasture_bytes": self.cfg.project_bytes,
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
                match scope {
                    Scope::Tree => "this sandbox has no tree scope: only a sandbox that can start other agents (paddock-host spawn), and the sandboxes it starts, have one",
                    Scope::Pasture => "a sandbox that another sandbox started has no pasture scope: use the tree scope, and the sandbox that started you decides what the pasture keeps",
                    _ => "this sandbox has no parent scope: only a sandbox that another sandbox started has one",
                },
            );
        };
        if write && scope == Scope::Parent {
            return Response::new(Status::Denied, "the parent scope is read-only");
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
        // A read of a namespace without a file finds nothing, and leaves no
        // file behind. A watch waits for the first write, so it opens it.
        let opened = ns.file(&self.root).and_then(|f| {
            if !write && req.op != "watch" && !f.exists() {
                Store::empty()
            } else {
                Store::open(&f)
            }
        });
        let mut st = match opened {
            Ok(s) => s,
            Err(e) => return Response::new(Status::Failed, format!("{e:#}")),
        };
        // The tree ends with its root. Its data stays for review, read-only.
        if write && scope == Scope::Tree {
            let root = self.id.tree.as_deref().unwrap_or_default();
            match self.registry().and_then(|r| r.ended(root)) {
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
        let r = self.run(&mut st, req, scope, &ns.key(), conn);
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
            let above = self.registry()?.ancestors(&e.writer)?;
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
    /// (without one, after now), or expires while the watch waits: a lease
    /// that runs out is free, and a watch on `lease:` wakes for it. Other
    /// sandboxes write through other processes, so this polls the store
    /// every 100 ms. When the time passes first, the status is `waiting`.
    fn watch(
        &self,
        st: &mut Store,
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
                // An expiry becomes a change in the history, so the cursor
                // also covers expiries between two watches.
                st.expire(ns, now)?;
                let changes = st.changes(ns, p, known, 1000)?;
                if !changes.is_empty() {
                    let cursor = changes.iter().map(|c| c.version).fold(known, u64::max);
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
                    Status::Waiting,
                    format!("still waiting: nothing changed in {timeout} seconds; run the same watch again"),
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

/// The pasture namespace of a sandbox: the pasture it joined, or its own.
pub fn pasture_ns(id: &Identity) -> Ns {
    match &id.pasture {
        Some(name) => Ns::Pasture {
            project: id.project.clone(),
            name: name.clone(),
        },
        None => Ns::Solo {
            project: id.project.clone(),
            sandbox: id.sandbox.clone(),
        },
    }
}

/// A running service. `finish` records the end of the run in the store.
pub struct Served {
    root: PathBuf,
    sandbox: String,
}

impl Served {
    pub fn finish(self) {
        if let Err(e) = Store::open(&shard::registry(&self.root))
            .and_then(|s| s.finish(&self.sandbox, crate::state::now()))
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
    shard::check_id(&svc.id.sandbox)?;
    let st = svc.registry()?;
    st.register(
        &svc.id.sandbox,
        &svc.id.harness,
        &svc.id.project,
        svc.id.parent.as_deref(),
        svc.id.pasture.as_deref(),
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
        root: svc.root.clone(),
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
    pasture: Option<String>,
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
        pasture,
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

/// What `paddock gc` can remove from paddock kv.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct GcPlan {
    /// Finished runs whose files (`self` and their own pasture, or the
    /// `tree` scope of a root) are older than `keep_days`.
    pub stale: Vec<store::Stale>,
    pub expired_entries: u64,
    pub expired_bytes: u64,
    /// The store of older paddock versions, when nothing changed it for
    /// `keep_days`: its size.
    pub legacy_bytes: Option<u64>,
}

fn mtime(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Every store file except the registry and the old store.
fn shard_files(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![root.join("user.db")];
    for dir in ["sandbox", "tree"] {
        out.extend(
            shard::ids_in(root, dir)
                .into_iter()
                .map(|id| root.join(dir).join(format!("{id}.db"))),
        );
    }
    for p in std::fs::read_dir(root.join("pasture"))
        .into_iter()
        .flatten()
        .flatten()
    {
        for f in std::fs::read_dir(p.path()).into_iter().flatten().flatten() {
            if f.path().extension().is_some_and(|e| e == "db") {
                out.push(f.path());
            }
        }
    }
    out.retain(|p| p.is_file());
    out
}

pub fn gc_plan(env: &HostEnv, running: &[String], now: u64) -> Result<GcPlan> {
    let root = kv_root(env);
    if !root.is_dir() {
        return Ok(GcPlan::default());
    }
    let cfg = KvCfg::load(env)?;
    let cutoff = now.saturating_sub(cfg.keep_days * 24 * 3600);
    let reg_file = shard::registry(&root);
    let reg = if reg_file.is_file() {
        Some(Store::open(&reg_file)?)
    } else {
        None
    };
    // A running child reads its parent and writes the tree of its root, so
    // the sandboxes above a running one stay too.
    let mut keep: Vec<String> = running.to_vec();
    if let Some(r) = &reg {
        for id in running {
            keep.extend(r.ancestors(id)?);
        }
    }
    let mut ids = shard::ids_in(&root, "sandbox");
    ids.extend(shard::ids_in(&root, "tree"));
    ids.sort();
    ids.dedup();
    let mut stale = Vec::new();
    for id in ids.into_iter().filter(|i| !keep.contains(i)) {
        let files = [
            root.join("sandbox").join(format!("{id}.db")),
            root.join("tree").join(format!("{id}.db")),
        ];
        let mut last = files.iter().map(|f| mtime(f)).max().unwrap_or(0);
        if let Some((started, ended)) = reg.as_ref().map(|r| r.times(&id)).transpose()?.flatten() {
            last = last.max(started).max(ended.unwrap_or(0));
        }
        if last >= cutoff {
            continue;
        }
        let (mut entries, mut bytes) = (0, 0);
        for f in files.iter().filter(|f| f.is_file()) {
            entries += Store::open(f)?.totals()?.0;
            bytes += shard::file_bytes(f);
        }
        stale.push(store::Stale {
            sandbox: id,
            entries,
            bytes,
            last,
        });
    }
    let (mut expired_entries, mut expired_bytes) = (0, 0);
    for f in shard_files(&root) {
        let (n, b) = Store::open(&f)?.expired(now)?;
        expired_entries += n;
        expired_bytes += b;
    }
    let legacy = shard::legacy(&root);
    let legacy_bytes =
        (legacy.is_file() && mtime(&legacy) < cutoff).then(|| shard::file_bytes(&legacy));
    Ok(GcPlan {
        stale,
        expired_entries,
        expired_bytes,
        legacy_bytes,
    })
}

/// The items of `paddock gc`: one for the files of finished runs, expired
/// keys and the old store. It is paddock's own data, so `--yes` alone
/// removes it.
pub fn gc_items(env: &HostEnv, now: u64) -> Result<Vec<crate::storage::Item>> {
    let running: Vec<String> = crate::state::Running::list(env)
        .into_iter()
        .map(|r| r.id)
        .collect();
    let plan = gc_plan(env, &running, now)?;
    if plan.stale.is_empty() && plan.expired_entries == 0 && plan.legacy_bytes.is_none() {
        return Ok(Vec::new());
    }
    let days = KvCfg::load(env)?.keep_days;
    let entries: u64 = plan.stale.iter().map(|s| s.entries).sum::<u64>() + plan.expired_entries;
    let mut what = format!(
        "paddock kv: {entries} keys of {} finished runs and expired keys",
        plan.stale.len()
    );
    if plan.legacy_bytes.is_some() {
        what.push_str(", and the store of older paddock versions");
    }
    Ok(vec![crate::storage::Item {
        what,
        tier: crate::storage::Tier::Auto,
        bytes: plan.stale.iter().map(|s| s.bytes).sum::<u64>()
            + plan.expired_bytes
            + plan.legacy_bytes.unwrap_or(0),
        reason: format!(
            "the self scope and the own pasture of runs, and the tree scope of spawn trees, that ended more than {days} days ago ([kv] keep_days), keys whose time to live passed, and the project scope of older paddock versions when nothing changed it for {days} days"
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
    let root = kv_root(env);
    let cfg = KvCfg::load(env)?;
    let cutoff = now.saturating_sub(cfg.keep_days * 24 * 3600);
    let mut n = 0;
    let mut reg = Store::open(&shard::registry(&root))?;
    for id in sandboxes.iter().filter(|s| !running.contains(s)) {
        shard::check_id(id)?;
        for dir in ["sandbox", "tree"] {
            let f = root.join(dir).join(format!("{id}.db"));
            if f.is_file() {
                n += Store::open(&f)?.totals()?.0;
                shard::remove_file(&f)?;
            }
        }
        reg.forget_sandbox(id)?;
    }
    for f in shard_files(&root) {
        n += Store::open(&f)?.purge_expired(now)?;
    }
    let legacy = shard::legacy(&root);
    if legacy.is_file() && mtime(&legacy) < cutoff {
        n += Store::open(&legacy)?.totals()?.0;
        shard::remove_file(&legacy)?;
    }
    reg.forget_ended(cutoff, &running)?;
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
    let root = kv_root(env);
    let user = match cfg.user {
        Mode::Ask => "asks at first use",
        Mode::Allow => "allowed",
        Mode::Deny => "off",
    };
    let reg = shard::registry(&root);
    let state = if reg.is_file() {
        match Store::open(&reg) {
            Ok(_) => format!("files in {}", root.display()),
            Err(e) => return vec![format!("  error    paddock kv: {e:#}")],
        }
    } else {
        "the files are created at the first start".to_string()
    };
    let mut out = vec![format!(
        "  on       paddock kv: {state}; user scope {user}; self data kept {} days after a run",
        cfg.keep_days
    )];
    if shard::legacy(&root).is_file() {
        out.push(format!(
            "  note     paddock kv: {} holds the project scope of an older paddock; sandboxes no longer read it. `paddock kv list --legacy` shows it, and `paddock gc` removes it {} days after its last change",
            shard::legacy(&root).display(),
            cfg.keep_days
        ));
    }
    out
}

#[cfg(test)]
mod tests;
