//! Credentials that paddock keeps for the sandboxes (`[credentials.NAME]`
//! of the user configuration).
//!
//! A real credential does not enter a sandbox by default. The user names
//! each one and the way the sandbox may use it:
//!
//! - `deliver`: the value enters the sandbox after the user allows it on
//!   the start card. It cannot be taken back.
//! - `gateway`: a local gateway adds the value to requests for the hosts
//!   that the credential is bound to; the sandbox sees a stand-in.
//! - `agent`: a protocol agent (ssh-agent, a git credential helper) uses
//!   the value on the host.
//! - `intercept`: TLS interception for tools that can do neither.
//!
//! Only `deliver` is built so far; the others are accepted in the
//! configuration and named as not available. A project `paddock.toml` can
//! never define a credential, and `from = "command"` runs on the host, like
//! `[exec]`. Values are read when they are needed, never written to disk.

use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    Agent,
    Gateway,
    Deliver,
    Intercept,
}

impl Via {
    pub fn name(self) -> &'static str {
        match self {
            Via::Agent => "agent",
            Via::Gateway => "gateway",
            Via::Deliver => "deliver",
            Via::Intercept => "intercept",
        }
    }
    /// Whether this paddock can use the credential this way.
    pub fn available(self) -> bool {
        self == Via::Deliver
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The environment of the paddock process, under the credential's name
    /// or `var`.
    #[default]
    Env,
    /// The output of `command`, run on the host.
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Confirm {
    /// Once for the run, on the start card.
    #[default]
    Session,
    /// At every use (ssh signatures).
    Each,
}

/// One `[credentials.NAME]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub via: Via,
    #[serde(default)]
    pub from: Source,
    /// `from = "env"`: the host variable to read. Default: the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub var: Option<String>,
    /// `from = "command"`: the program and its arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// gateway, intercept: the hosts that the credential is bound to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// gateway, intercept: path prefixes on those hosts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// gateway: the variable in the sandbox that names the gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url_env: Option<String>,
    /// deliver: the variable in the sandbox that holds the value. Default:
    /// the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// Only for these harnesses. Empty: every harness.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harnesses: Vec<String>,
    /// agent (ssh): the public keys whose private keys the sandbox may use.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    #[serde(default)]
    pub confirm: Confirm,
}

impl Credential {
    /// The variable that holds it in the sandbox (deliver).
    pub fn sandbox_var<'a>(&'a self, name: &'a str) -> &'a str {
        self.env.as_deref().unwrap_or(name)
    }
    /// The host variable it comes from (`from = "env"`).
    pub fn host_var<'a>(&'a self, name: &'a str) -> &'a str {
        self.var.as_deref().unwrap_or(name)
    }
    pub fn applies_to(&self, harness: &str) -> bool {
        self.harnesses.is_empty() || self.harnesses.iter().any(|h| h == harness)
    }
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.starts_with(|c: char| c.is_ascii_alphabetic())
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn valid_var(n: &str) -> bool {
    !n.is_empty()
        && n.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `[credentials.*]` of the user configuration.
pub fn parse(text: &str) -> Result<BTreeMap<String, Credential>> {
    let v: toml::Value = toml::from_str(text)?;
    let Some(t) = v.get("credentials") else {
        return Ok(BTreeMap::new());
    };
    let all: BTreeMap<String, Credential> = t.clone().try_into()?;
    for (name, c) in &all {
        if !valid_name(name) {
            bail!("'{name}' is not a credential name: use letters, digits, '_' and '-', starting with a letter");
        }
        match c.from {
            Source::Command if c.command.is_empty() => {
                bail!("[credentials.{name}] has from = \"command\" and no command")
            }
            Source::Env if !c.command.is_empty() => {
                bail!("[credentials.{name}] has a command; set from = \"command\" to use it")
            }
            _ => {}
        }
        for v in [
            c.var.as_deref(),
            c.env.as_deref(),
            c.base_url_env.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !valid_var(v) {
                bail!("[credentials.{name}]: '{v}' is not an environment variable name");
            }
        }
        if c.via == Via::Deliver && c.env.is_none() && !valid_var(name) {
            bail!("[credentials.{name}]: name the variable for the sandbox with env = \"...\"");
        }
        if matches!(c.via, Via::Gateway | Via::Intercept) && c.hosts.is_empty() {
            bail!(
                "[credentials.{name}]: via = \"{}\" needs hosts",
                c.via.name()
            );
        }
    }
    Ok(all)
}

pub fn load(env: &HostEnv) -> Result<BTreeMap<String, Credential>> {
    let file = env.paddock_config().join("config.toml");
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Ok(BTreeMap::new());
    };
    parse(&text).with_context(|| format!("[credentials] in {} is invalid", file.display()))
}

/// Whether the source of a credential is there, without reading a value
/// from a command: an env source must be set, a command must exist.
pub fn source_ready(env: &HostEnv, name: &str, c: &Credential) -> bool {
    match c.from {
        Source::Env => env.var(c.host_var(name)).is_some_and(|v| !v.is_empty()),
        Source::Command => c
            .command
            .first()
            .is_some_and(|p| p.contains('/') || env.which(p).is_some()),
    }
}

/// The host variables that registered credentials come from: they never
/// pass with the inherited environment.
pub fn host_vars(all: &BTreeMap<String, Credential>) -> Vec<String> {
    all.iter()
        .filter(|(_, c)| c.from == Source::Env)
        .map(|(n, c)| c.host_var(n).to_string())
        .collect()
}

/// The capability name on the approval card.
pub const CAPABILITY: &str = "credentials";
/// The most bytes that a command source may print.
const MAX_VALUE: usize = 64 * 1024;
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The credentials of a run: what to deliver, and what to say about the rest.
#[derive(Debug, Default)]
pub struct Plan {
    /// `deliver` credentials whose source is there, by name.
    pub deliver: Vec<(String, Credential)>,
    pub notices: Vec<String>,
}

/// The registered credentials for a harness. A spawned sandbox gets none:
/// it would ask the user again for each child.
pub fn plan(env: &HostEnv, harness: &str, spawned: bool) -> Result<Plan> {
    let mut p = Plan::default();
    for (name, c) in load(env)? {
        if !c.applies_to(harness) {
            continue;
        }
        if !c.via.available() {
            p.notices.push(format!(
                "credential {name}: via = \"{}\" is not available in this paddock yet; the sandbox does not get it",
                c.via.name()
            ));
            continue;
        }
        if spawned {
            p.notices.push(format!(
                "credential {name}: a sandbox started by another one gets no credentials"
            ));
            continue;
        }
        if !source_ready(env, &name, &c) {
            p.notices.push(match c.from {
                Source::Env => format!(
                    "credential {name}: {} is not set where paddock started",
                    c.host_var(&name)
                ),
                Source::Command => format!(
                    "credential {name}: the program {} is not there",
                    c.command[0]
                ),
            });
            continue;
        }
        p.deliver.push((name, c));
    }
    Ok(p)
}

fn describe(name: &str, c: &Credential) -> String {
    let from = match c.from {
        Source::Env => format!("from the host variable {}", c.host_var(name)),
        Source::Command => format!("from the command `{}`", c.command.join(" ")),
    };
    format!(
        "{name}: {from}, as the variable {} in the sandbox",
        c.sandbox_var(name)
    )
}

/// Ask the user twice: allow, then confirm. Only both answers deliver.
/// Every step goes to the audit log, never a value.
pub fn ask(
    env: &HostEnv,
    spec: &crate::spec::SandboxSpec,
    approver: &dyn crate::broker::Approver,
    wait: std::time::Duration,
    plan: &Plan,
) -> bool {
    if plan.deliver.is_empty() {
        return false;
    }
    let names: Vec<&str> = plan.deliver.iter().map(|(n, _)| n.as_str()).collect();
    let list: Vec<String> = plan.deliver.iter().map(|(n, c)| describe(n, c)).collect();
    let card = |step: &str, summary: String, detail: String| crate::broker::Pending {
        id: format!("cred-{}-{step}-{}", spec.id, crate::state::now()),
        sandbox: spec.id.clone(),
        harness: spec.harness.clone(),
        project: spec.project.clone(),
        capability: CAPABILITY.into(),
        summary,
        detail,
        reason: String::new(),
        created: crate::state::now(),
        expires: crate::state::now() + wait.as_secs(),
        session_allowed: false,
        started_by: Vec::new(),
        choices: Vec::new(),
    };
    let first = card(
        "allow",
        format!("give {} to this sandbox", names.join(", ")),
        format!(
            "{}\n\nThe values enter the sandbox, and the agent can read them. They cannot be taken back. The harness starts after your answer.",
            list.join("\n")
        ),
    );
    let second = card(
        "confirm",
        format!("confirm: hand over {}", names.join(", ")),
        format!(
            "Second step. Yes hands over {} for this run. No starts the sandbox without them.",
            names.join(", ")
        ),
    );
    for (step, p) in [("allowed", &first), ("confirmed", &second)] {
        let answer = approver.ask(env, p, wait);
        let decision = match answer {
            Some(crate::broker::Answer::Deny) => "denied",
            None => "timed out",
            Some(_) => step,
        };
        audit(env, spec, &names.join(", "), decision);
        if !matches!(
            answer,
            Some(crate::broker::Answer::Once | crate::broker::Answer::Session)
        ) {
            return false;
        }
    }
    true
}

/// Read the values. A source that fails is left out, with a notice.
pub fn read(env: &HostEnv, plan: &Plan) -> (Vec<(String, String, String)>, Vec<String>) {
    let mut out = Vec::new();
    let mut notices = Vec::new();
    for (name, c) in &plan.deliver {
        let value = match c.from {
            Source::Env => env
                .var(c.host_var(name))
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("{} is not set", c.host_var(name))),
            Source::Command => run_command(&c.command),
        };
        match value {
            Ok(v) => out.push((name.clone(), c.sandbox_var(name).to_string(), v)),
            Err(e) => notices.push(format!("credential {name} was not delivered: {e:#}")),
        }
    }
    (out, notices)
}

fn run_command(cmd: &[String]) -> Result<String> {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot run {}", cmd[0]))?;
    let mut out = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut out).take(MAX_VALUE as u64 + 1).read_to_end(&mut buf);
        buf
    });
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > COMMAND_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{} did not finish in {} seconds",
                cmd[0],
                COMMAND_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let buf = reader.join().unwrap_or_default();
    if !status.success() {
        bail!("{} failed ({status})", cmd[0]);
    }
    if buf.len() > MAX_VALUE {
        bail!("{} printed more than {MAX_VALUE} bytes", cmd[0]);
    }
    let mut v = String::from_utf8(buf).context("the value is not UTF-8")?;
    if v.ends_with('\n') {
        v.pop();
        if v.ends_with('\r') {
            v.pop();
        }
    }
    if v.is_empty() {
        bail!("{} printed nothing", cmd[0]);
    }
    Ok(v)
}

/// One line in the audit log of host requests. Names only.
pub fn audit(env: &HostEnv, spec: &crate::spec::SandboxSpec, names: &str, decision: &str) {
    crate::broker::audit_line(
        env,
        &serde_json::json!({
            "time": crate::state::now(),
            "sandbox": spec.id,
            "harness": spec.harness,
            "project": spec.project,
            "capability": CAPABILITY,
            "detail": format!("deliver {names}"),
            "decision": decision,
        }),
    );
}

/// One registered credential for `paddock credentials list`. No value.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub via: &'static str,
    pub available: bool,
    pub source: String,
    pub ready: bool,
    pub harnesses: Vec<String>,
    pub delivered: u64,
    pub last_delivered: Option<u64>,
}

pub fn rows(env: &HostEnv) -> Result<Vec<Row>> {
    let all = load(env)?;
    // Deliveries from the audit log of host requests.
    let mut used: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(crate::broker::audit_file(env)) {
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v["capability"] != CAPABILITY || v["decision"] != "delivered" {
                continue;
            }
            if let Some(name) = v["detail"]
                .as_str()
                .and_then(|d| d.strip_prefix("deliver "))
            {
                let e = used.entry(name.to_string()).or_default();
                e.0 += 1;
                e.1 = e.1.max(v["time"].as_u64().unwrap_or(0));
            }
        }
    }
    Ok(all
        .iter()
        .map(|(name, c)| {
            let (n, last) = used.get(name).copied().unwrap_or_default();
            Row {
                name: name.clone(),
                via: c.via.name(),
                available: c.via.available(),
                source: match c.from {
                    Source::Env => format!("env {}", c.host_var(name)),
                    Source::Command => format!("command {}", c.command[0]),
                },
                ready: source_ready(env, name, c),
                harnesses: c.harnesses.clone(),
                delivered: n,
                last_delivered: (last > 0).then_some(last),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_forms() {
        let all = parse(
            r#"
[credentials.GITHUB_TOKEN]
via = "gateway"
hosts = ["api.github.com"]
paths = ["/repos/", "/user"]
base_url_env = "GH_API_URL"
harnesses = ["claude", "codex"]

[credentials.DB_PASSWORD]
via = "deliver"
from = "command"
command = ["secret-tool", "lookup", "service", "mydb"]

[credentials.ssh]
via = "agent"
keys = ["~/.ssh/id_ed25519.pub"]
confirm = "each"
"#,
        );
        // ssh has no variable name of its own for deliver, but it is an agent.
        let all = all.unwrap();
        assert_eq!(all["GITHUB_TOKEN"].via, Via::Gateway);
        assert!(all["GITHUB_TOKEN"].applies_to("codex"));
        assert!(!all["GITHUB_TOKEN"].applies_to("gemini"));
        assert_eq!(all["DB_PASSWORD"].from, Source::Command);
        assert_eq!(all["DB_PASSWORD"].sandbox_var("DB_PASSWORD"), "DB_PASSWORD");
        assert_eq!(all["ssh"].confirm, Confirm::Each);
        assert!(Via::Deliver.available() && !Via::Gateway.available());
        assert!(parse("").unwrap().is_empty());
    }

    #[test]
    fn rejects_unclear_entries() {
        for bad in [
            "[credentials.x]\nvia = \"teleport\"\n",
            "[credentials.x]\nvia = \"deliver\"\nfrom = \"command\"\n",
            "[credentials.x]\nvia = \"deliver\"\ncommand = [\"pass\"]\n",
            "[credentials.x]\nvia = \"gateway\"\n",
            "[credentials.x]\nvia = \"deliver\"\nenv = \"A-B\"\n",
            "[credentials.my-db]\nvia = \"deliver\"\n",
            "[credentials.x]\nvia = \"deliver\"\nother = 1\n",
            "[credentials.\"1x\"]\nvia = \"deliver\"\n",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        parse("[credentials.my-db]\nvia = \"deliver\"\nenv = \"DB\"\n").unwrap();
    }

    struct Answers(
        std::sync::Mutex<Vec<Option<crate::broker::Answer>>>,
        std::sync::Mutex<usize>,
    );
    impl crate::broker::Approver for Answers {
        fn ask(
            &self,
            _: &HostEnv,
            p: &crate::broker::Pending,
            _: std::time::Duration,
        ) -> Option<crate::broker::Answer> {
            assert_eq!(p.capability, CAPABILITY);
            assert!(!p.session_allowed);
            *self.1.lock().unwrap() += 1;
            self.0.lock().unwrap().remove(0)
        }
    }

    fn world(cfg: &str, vars: &[(&str, &str)]) -> (tempfile::TempDir, HostEnv) {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        std::fs::create_dir_all(home.join(".config/paddock")).unwrap();
        std::fs::write(home.join(".config/paddock/config.toml"), cfg).unwrap();
        let mut v = BTreeMap::new();
        v.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
        for (k, val) in vars {
            v.insert(k.to_string(), val.to_string());
        }
        let env = HostEnv::from_parts(home, "t".into(), 1000, 1000, v);
        (t, env)
    }

    fn spec(env: &HostEnv) -> crate::spec::SandboxSpec {
        crate::spec::SandboxSpec::new(
            "sb-1".into(),
            "claude",
            std::path::Path::new("/home/demo/app"),
            env.home.join("sb"),
            &env.home,
            1000,
            1000,
            "t",
        )
    }

    const CFG: &str = r#"
[credentials.DB_PASSWORD]
via = "deliver"
var = "HOST_DB"

[credentials.FROM_CMD]
via = "deliver"
from = "command"
command = ["sh", "-c", "printf 'line1\nline2\n'"]

[credentials.ONLY_CODEX]
via = "deliver"
harnesses = ["codex"]

[credentials.LATER]
via = "gateway"
hosts = ["api.example.com"]

[credentials.MISSING]
via = "deliver"
"#;

    #[test]
    fn plan_reads_only_what_is_there_and_names_the_rest() {
        let (_t, env) = world(CFG, &[("HOST_DB", "s3cret")]);
        let p = plan(&env, "claude", false).unwrap();
        let names: Vec<&str> = p.deliver.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["DB_PASSWORD", "FROM_CMD"]);
        let notes = p.notices.join("\n");
        assert!(
            notes.contains("LATER") && notes.contains("not available"),
            "{notes}"
        );
        assert!(
            notes.contains("MISSING") && notes.contains("not set"),
            "{notes}"
        );
        assert!(!notes.contains("ONLY_CODEX"));
        let (values, notes) = read(&env, &p);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(
            values,
            [
                (
                    "DB_PASSWORD".to_string(),
                    "DB_PASSWORD".to_string(),
                    "s3cret".to_string()
                ),
                (
                    "FROM_CMD".to_string(),
                    "FROM_CMD".to_string(),
                    "line1\nline2".to_string()
                ),
            ]
        );
        // A spawned sandbox gets none.
        assert!(plan(&env, "claude", true).unwrap().deliver.is_empty());
    }

    #[test]
    fn a_failing_command_delivers_nothing() {
        let (_t, env) = world(
            "[credentials.X]\nvia = \"deliver\"\nfrom = \"command\"\ncommand = [\"sh\", \"-c\", \"exit 3\"]\n",
            &[],
        );
        let p = plan(&env, "claude", false).unwrap();
        let (values, notes) = read(&env, &p);
        assert!(values.is_empty());
        assert!(notes[0].contains("failed"), "{notes:?}");
    }

    #[test]
    fn only_two_yes_answers_deliver_and_the_log_has_no_value() {
        use crate::broker::Answer::*;
        let (_t, env) = world(CFG, &[("HOST_DB", "s3cret")]);
        let p = plan(&env, "claude", false).unwrap();
        let s = spec(&env);
        let wait = std::time::Duration::from_secs(1);
        for (answers, delivered, asked) in [
            (vec![Some(Once), Some(Once)], true, 2),
            (vec![Some(Once), Some(Deny)], false, 2),
            (vec![Some(Deny)], false, 1),
            (vec![None], false, 1),
        ] {
            let a = Answers(std::sync::Mutex::new(answers), std::sync::Mutex::new(0));
            assert_eq!(ask(&env, &s, &a, wait, &p), delivered);
            assert_eq!(*a.1.lock().unwrap(), asked);
        }
        let log = std::fs::read_to_string(crate::broker::audit_file(&env)).unwrap();
        assert!(
            log.contains("\"confirmed\"")
                && log.contains("\"denied\"")
                && log.contains("timed out")
        );
        assert!(!log.contains("s3cret"));
    }

    #[test]
    fn rows_count_deliveries_from_the_log() {
        let (_t, env) = world(CFG, &[("HOST_DB", "s3cret")]);
        let s = spec(&env);
        audit(&env, &s, "DB_PASSWORD", "delivered");
        audit(&env, &s, "DB_PASSWORD", "delivered");
        let rows = rows(&env).unwrap();
        let db = rows.iter().find(|r| r.name == "DB_PASSWORD").unwrap();
        assert_eq!((db.delivered, db.ready, db.available), (2, true, true));
        assert_eq!(db.source, "env HOST_DB");
        let later = rows.iter().find(|r| r.name == "LATER").unwrap();
        assert!(!later.available);
        assert!(!serde_json::to_string(&rows).unwrap().contains("s3cret"));
    }
}
