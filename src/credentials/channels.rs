//! The agent channels for docker registries and AWS (`via = "agent"` with
//! `kind = "docker"` or `kind = "aws"`).
//!
//! The sandbox asks the host broker (`paddock-host credential`). The
//! broker of the run hands the request to `Channels`, which checks that the
//! credential covers it, asks the user at the first use in the run, and
//! gets the login from the user's own tools on the host:
//!
//! - docker: the user's docker credential store (`credHelpers`, then
//!   `credsStore`, then plain `auths` of `~/.docker/config.json`), only for
//!   the listed registries. The login reaches the sandbox's docker client.
//! - aws: short-lived credentials only (`aws sts get-session-token`, or
//!   `aws configure export-credentials` when the profile yields temporary
//!   credentials already). The long-lived keys of the profile never leave
//!   the host.
//!
//! The audit log keeps the credential, the registry or the profile, and
//! the result. Never a value.

use super::{secret::Secret, Credential, Kind};
use crate::broker::Status;
use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The broker capability of the sandbox requests.
pub const CAPABILITY: &str = "credential";
/// Where the docker configuration of the sandbox appears.
pub const DOCKER_CONFIG_IN_SANDBOX: &str = "/run/paddock/docker";
/// The name of the credential helper in the sandbox's docker configuration.
pub const DOCKER_HELPER: &str = "paddock";
const MAX_OUTPUT: usize = 64 * 1024;
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);

/// The host of a registry name or a server URL (`ghcr.io`,
/// `https://ghcr.io/v2/`), lowercase. Docker Hub has one name for all of
/// its forms. None: not a registry.
pub fn registry_host(s: &str) -> Option<String> {
    let s = s.trim();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let host = s.split('/').next()?.to_ascii_lowercase();
    let (name, port) = match host.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (host.as_str(), None),
    };
    if name.is_empty()
        || name.len() > 253
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        || port.is_some_and(|p| p.parse::<u16>().is_err())
    {
        return None;
    }
    Some(match host.as_str() {
        "docker.io" | "registry-1.docker.io" | "index.docker.io" => "index.docker.io".into(),
        _ => host,
    })
}

/// The docker configuration of the user on the host.
pub fn docker_config_file(env: &HostEnv) -> PathBuf {
    match env.var("DOCKER_CONFIG").filter(|d| !d.is_empty()) {
        Some(d) => Path::new(d).join("config.json"),
        None => env.home.join(".docker/config.json"),
    }
}

/// The docker configuration of the sandbox: the paddock helper for exactly
/// these registries.
pub fn sandbox_docker_config(registries: &[String]) -> String {
    let mut helpers = serde_json::Map::new();
    for r in registries {
        helpers.insert(r.clone(), DOCKER_HELPER.into());
        // Docker names Docker Hub by its old index URL.
        if registry_host(r).as_deref() == Some("index.docker.io") {
            helpers.insert("https://index.docker.io/v1/".into(), DOCKER_HELPER.into());
        }
    }
    let v = serde_json::json!({ "credHelpers": helpers });
    serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"
}

fn valid_store(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !n.starts_with('.')
}

/// A login for a registry: the user name and the secret (a password or a
/// token).
pub struct Login {
    pub username: String,
    pub secret: Secret,
}

/// The user's docker login for `host`, from the docker configuration of
/// the host. `server` is what the sandbox's docker client asked for, and
/// goes to the credential helper as docker sends it.
pub fn docker_login(env: &HostEnv, server: &str, host: &str) -> Result<Login> {
    let file = docker_config_file(env);
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("cannot read {}", file.display()))?;
    let cfg: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("{} is not JSON", file.display()))?;
    let matches = |k: &str| registry_host(k).as_deref() == Some(host);
    let helper = cfg["credHelpers"]
        .as_object()
        .and_then(|m| m.iter().find(|(k, _)| matches(k)))
        .and_then(|(_, v)| v.as_str())
        .or_else(|| cfg["credsStore"].as_str().filter(|s| !s.is_empty()));
    if let Some(store) = helper {
        if !valid_store(store) {
            bail!(
                "'{store}' in {} is not a credential helper name",
                file.display()
            );
        }
        let prog = format!("docker-credential-{store}");
        let exe = env
            .which(&prog)
            .with_context(|| format!("the docker credential helper {prog} is not there"))?;
        let out = run_tool(&exe, &["get"], server.as_bytes(), &[])
            .with_context(|| format!("{prog} has no login for {host}"))?;
        let v: serde_json::Value = serde_json::from_slice(out.as_bytes())
            .with_context(|| format!("{prog} printed no JSON"))?;
        let (Some(user), Some(secret)) = (v["Username"].as_str(), v["Secret"].as_str()) else {
            bail!("{prog} printed no Username and Secret");
        };
        if secret.is_empty() {
            bail!("{prog} has no login for {host}");
        }
        return Ok(Login {
            username: user.to_string(),
            secret: Secret::from(secret),
        });
    }
    let entry = cfg["auths"]
        .as_object()
        .and_then(|m| m.iter().find(|(k, _)| matches(k)))
        .map(|(_, v)| v)
        .with_context(|| {
            format!("your docker configuration has no login for {host}; run docker login {host} on the host")
        })?;
    if let Some(t) = entry["identitytoken"].as_str().filter(|t| !t.is_empty()) {
        return Ok(Login {
            username: "<token>".into(),
            secret: Secret::from(t),
        });
    }
    if let Some(a) = entry["auth"].as_str().filter(|a| !a.is_empty()) {
        use base64::Engine as _;
        let raw = Secret::new(
            base64::engine::general_purpose::STANDARD
                .decode(a)
                .context("the auth value is not base64")?,
        );
        let text = std::str::from_utf8(raw.as_bytes()).context("the auth value is not UTF-8")?;
        let (user, pass) = text
            .split_once(':')
            .context("the auth value has no user name")?;
        return Ok(Login {
            username: user.to_string(),
            secret: Secret::from(pass),
        });
    }
    match (entry["username"].as_str(), entry["password"].as_str()) {
        (Some(u), Some(p)) if !p.is_empty() => Ok(Login {
            username: u.to_string(),
            secret: Secret::from(p),
        }),
        _ => bail!("your docker configuration has no login for {host}; run docker login {host} on the host"),
    }
}

/// Run a host tool with `stdin`, for at most TOOL_TIMEOUT. Its output is a
/// secret. stderr goes nowhere: it can hold a value too.
fn run_tool(exe: &Path, args: &[&str], stdin: &[u8], envs: &[(&str, &str)]) -> Result<Secret> {
    use std::io::{Read as _, Write as _};
    use std::process::{Command, Stdio};
    let name = exe
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut child = Command::new(exe)
        .args(args)
        .envs(envs.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot run {name}"))?;
    let mut input = child.stdin.take().expect("piped");
    let data = stdin.to_vec();
    std::thread::spawn(move || {
        let _ = input.write_all(&data);
    });
    let mut out = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut out).take(MAX_OUTPUT as u64 + 1).read_to_end(&mut buf);
        buf
    });
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > TOOL_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{name} did not finish in {} seconds",
                TOOL_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let buf = Secret::new(reader.join().unwrap_or_default());
    if !status.success() {
        bail!("{name} failed ({status})");
    }
    if buf.len() > MAX_OUTPUT {
        bail!("{name} printed more than {MAX_OUTPUT} bytes");
    }
    Ok(buf)
}

/// Asks the user at the first use: the summary and the detail of the card.
/// True: allowed for the rest of the run.
pub type Ask = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;

/// The docker and aws credentials of one run, served for the broker.
pub struct Channels {
    pub env: HostEnv,
    pub spec: crate::spec::SandboxSpec,
    pub creds: Vec<(String, Credential)>,
    /// The revoke file of the run (`credentials-revoked`).
    pub revoked: PathBuf,
    pub ask: Ask,
    /// The uses that the user allowed in this run.
    allowed: Mutex<BTreeSet<String>>,
    /// One card at a time, so that two first uses ask once.
    asking: Mutex<()>,
}

impl Channels {
    pub fn new(
        env: HostEnv,
        spec: crate::spec::SandboxSpec,
        creds: Vec<(String, Credential)>,
        revoked: PathBuf,
        ask: Ask,
    ) -> Self {
        Self {
            env,
            spec,
            creds,
            revoked,
            ask,
            allowed: Mutex::new(BTreeSet::new()),
            asking: Mutex::new(()),
        }
    }

    fn is_revoked(&self, name: &str) -> bool {
        std::fs::read_to_string(&self.revoked)
            .map(|t| t.lines().any(|l| l.trim() == name || l.trim() == "*"))
            .unwrap_or(false)
    }

    /// Whether the user allows `key` in this run: asked at the first use.
    fn permit(&self, key: &str, summary: &str, detail: &str) -> bool {
        if self.allowed.lock().unwrap().contains(key) {
            return true;
        }
        let _one = self.asking.lock().unwrap();
        if self.allowed.lock().unwrap().contains(key) {
            return true;
        }
        if !(self.ask)(summary, detail) {
            return false;
        }
        self.allowed.lock().unwrap().insert(key.to_string());
        true
    }

    fn audit(&self, name: &str, what: &str, decision: &str) {
        audit(&self.env, &self.spec, name, what, decision);
    }

    /// One request of the sandbox. Ok: the result for the sandbox's tool.
    pub fn handle(
        &self,
        args: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> std::result::Result<serde_json::Value, (Status, String)> {
        let arg = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");
        match arg("kind") {
            "docker" => self.docker(arg("server")),
            "aws" => self.aws(arg("name")),
            other => Err((
                Status::Invalid,
                format!("unknown credential kind '{other}'"),
            )),
        }
    }

    fn docker(&self, server: &str) -> std::result::Result<serde_json::Value, (Status, String)> {
        if server.len() > 512 || server.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err((Status::Invalid, "not a registry address".into()));
        }
        let Some(host) = registry_host(server) else {
            return Err((Status::Invalid, format!("'{server}' is not a registry")));
        };
        let what = format!("docker {host}");
        let Some((name, _)) = self.creds.iter().find(|(_, c)| {
            c.kind() == Kind::Docker
                && c.registries
                    .iter()
                    .any(|r| registry_host(r).as_deref() == Some(host.as_str()))
        }) else {
            self.audit("-", &what, "denied: not a listed registry");
            return Err((
                Status::Denied,
                format!("{host} is not a registry of a docker credential of this sandbox"),
            ));
        };
        if self.is_revoked(name) {
            self.audit(name, &what, "denied: revoked");
            return Err((Status::Denied, format!("the credential {name} was revoked")));
        }
        if !self.permit(
            &format!("docker\0{name}\0{host}"),
            &format!("docker login for {host} ({name})"),
            &format!(
                "The sandbox's docker client asks for your login to {host}.\nCredential: {name}\nYes gives the login to the docker client in the sandbox for this request and for later requests to {host} in this run. paddock credentials revoke ends it."
            ),
        ) {
            self.audit(name, &what, "denied by the user");
            return Err((Status::Denied, "the user denied the docker login".into()));
        }
        match docker_login(&self.env, server, &host) {
            Ok(l) => {
                self.audit(name, &what, "allowed");
                Ok(serde_json::json!({
                    "ServerURL": server,
                    "Username": l.username,
                    "Secret": l.secret.as_str(),
                }))
            }
            Err(e) => {
                self.audit(name, &what, "failed");
                Err((Status::Failed, format!("{e:#}")))
            }
        }
    }
}

impl Channels {
    fn aws(&self, name: &str) -> std::result::Result<serde_json::Value, (Status, String)> {
        let Some((name, c)) = self
            .creds
            .iter()
            .find(|(n, c)| n == name && c.kind() == Kind::Aws)
        else {
            return Err((
                Status::Invalid,
                format!("{name} is not an aws credential of this sandbox"),
            ));
        };
        let profile = c.profile.as_deref().unwrap_or_default();
        let what = format!("aws {profile}");
        if self.is_revoked(name) {
            self.audit(name, &what, "denied: revoked");
            return Err((Status::Denied, format!("the credential {name} was revoked")));
        }
        if !self.permit(
            &format!("aws\0{name}"),
            &format!("AWS credentials of the profile {profile} ({name})"),
            &format!(
                "The sandbox asks for AWS credentials of your profile {profile}.\nCredential: {name}\nYes gives it credentials that end after {} seconds, now and at each later request in this run, without a new question. The long-lived keys stay on the host. paddock credentials revoke ends it.",
                c.aws_duration()
            ),
        ) {
            self.audit(name, &what, "denied by the user");
            return Err((Status::Denied, "the user denied the AWS credentials".into()));
        }
        match aws_credentials(&self.env, profile, c.aws_duration()) {
            Ok(v) => {
                self.audit(name, &what, "allowed");
                Ok(v)
            }
            Err(e) => {
                self.audit(name, &what, "failed");
                Err((Status::Failed, format!("{e:#}")))
            }
        }
    }
}

/// The `credential_process` output for temporary credentials. None: a
/// field is missing, or the set has no session token or no expiration,
/// which means long-lived keys.
fn process_output(v: &serde_json::Value) -> Option<serde_json::Value> {
    let f = |k: &str| v[k].as_str().filter(|s| !s.is_empty());
    Some(serde_json::json!({
        "Version": 1,
        "AccessKeyId": f("AccessKeyId")?,
        "SecretAccessKey": f("SecretAccessKey")?,
        "SessionToken": f("SessionToken")?,
        "Expiration": f("Expiration")?,
    }))
}

/// Short-lived credentials of a profile of the user's AWS configuration.
/// A profile that yields temporary credentials already (SSO, an assumed
/// role) gives them as they are. Else STS makes a session from the
/// profile's keys. The long-lived keys are never returned.
pub fn aws_credentials(env: &HostEnv, profile: &str, duration: u64) -> Result<serde_json::Value> {
    if !valid_profile(profile) {
        bail!("'{profile}' is not an AWS profile name");
    }
    let exe = env.which("aws").context("the program aws is not there")?;
    let envs = [("AWS_PAGER", "")];
    if let Ok(out) = run_tool(
        &exe,
        &[
            "configure",
            "export-credentials",
            "--profile",
            profile,
            "--format",
            "process",
        ],
        b"",
        &envs,
    ) {
        if let Some(v) = serde_json::from_slice::<serde_json::Value>(out.as_bytes())
            .ok()
            .as_ref()
            .and_then(process_output)
        {
            return Ok(v);
        }
    }
    let d = duration
        .clamp(super::AWS_DURATION.0, super::AWS_DURATION.1)
        .to_string();
    let out = run_tool(
        &exe,
        &[
            "sts",
            "get-session-token",
            "--duration-seconds",
            &d,
            "--profile",
            profile,
            "--output",
            "json",
        ],
        b"",
        &envs,
    )
    .with_context(|| format!("aws sts get-session-token for the profile {profile}"))?;
    let v: serde_json::Value =
        serde_json::from_slice(out.as_bytes()).context("aws sts printed no JSON")?;
    process_output(&v["Credentials"]).context("aws sts printed no session credentials")
}

/// Where the AWS configuration of the sandbox appears.
pub const AWS_IN_SANDBOX: &str = "/run/paddock/aws";

/// The AWS configuration of the sandbox: one profile per aws credential,
/// each with `credential_process = paddock-host credential aws NAME`.
pub fn sandbox_aws_config(creds: &[(String, Credential)]) -> String {
    let mut out = String::new();
    for (name, c) in creds.iter().filter(|(_, c)| c.kind() == Kind::Aws) {
        let p = c.profile.as_deref().unwrap_or_default();
        out.push_str(&format!(
            "[profile {p}]\ncredential_process = paddock-host credential aws {name}\n\n"
        ));
    }
    out
}

/// Give the sandbox its AWS configuration: `AWS_CONFIG_FILE`, `AWS_PROFILE`
/// (the first aws credential), and an empty shared credentials file.
pub fn setup_aws(
    spec: &mut crate::spec::SandboxSpec,
    run_dir: &Path,
    creds: &[(String, Credential)],
) -> Result<()> {
    let Some((_, first)) = creds.iter().find(|(_, c)| c.kind() == Kind::Aws) else {
        return Ok(());
    };
    let dir = run_dir.join("aws-config");
    crate::privfs::dir(&dir)?;
    crate::gui::write_file(&dir.join("config"), &sandbox_aws_config(creds), 0o644)?;
    crate::gui::write_file(&dir.join("credentials"), "", 0o644)?;
    spec.add_mount(crate::spec::Mount::bind(
        &dir,
        AWS_IN_SANDBOX,
        false,
        "credentials: aws configuration",
    ));
    let origin = "credentials: aws credential_process";
    spec.set_env(
        "AWS_CONFIG_FILE",
        format!("{AWS_IN_SANDBOX}/config"),
        origin,
    );
    spec.set_env(
        "AWS_SHARED_CREDENTIALS_FILE",
        format!("{AWS_IN_SANDBOX}/credentials"),
        origin,
    );
    spec.set_env(
        "AWS_PROFILE",
        first.profile.clone().unwrap_or_default(),
        origin,
    );
    for k in [
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_DEFAULT_PROFILE",
    ] {
        spec.env.remove(k);
    }
    Ok(())
}

/// The approval card of a first use.
pub fn approver_ask(
    env: HostEnv,
    spec: crate::spec::SandboxSpec,
    approver: Arc<dyn crate::broker::Approver>,
    wait: Duration,
) -> Ask {
    Arc::new(move |summary: &str, detail: &str| {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let now = crate::state::now();
        let p = crate::broker::Pending {
            id: format!(
                "credential-{}-{now}-{}",
                spec.id,
                SEQ.fetch_add(1, Ordering::SeqCst)
            ),
            sandbox: spec.id.clone(),
            harness: spec.harness.clone(),
            project: spec.project.clone(),
            capability: super::CAPABILITY.into(),
            summary: summary.to_string(),
            detail: detail.to_string(),
            reason: String::new(),
            created: now,
            expires: now + wait.as_secs(),
            session_allowed: false,
            started_by: Vec::new(),
            choices: Vec::new(),
        };
        matches!(
            approver.ask(&env, &p, wait),
            Some(crate::broker::Answer::Once | crate::broker::Answer::Session)
        )
    })
}

/// One use in the audit log of host requests: the credential, the registry
/// or the profile, and the result. An allowed use counts in `paddock
/// credentials list`.
pub fn audit(env: &HostEnv, spec: &crate::spec::SandboxSpec, name: &str, what: &str, result: &str) {
    crate::broker::audit_line(
        env,
        &serde_json::json!({
            "time": crate::state::now(),
            "sandbox": spec.id,
            "harness": spec.harness,
            "project": spec.project,
            "capability": super::CAPABILITY,
            "detail": format!("agent {name} {what}"),
            "decision": result,
        }),
    );
}

/// An AWS profile name.
pub fn valid_profile(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 64
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

#[cfg(test)]
mod tests;
