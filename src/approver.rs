//! The agent approver: a command on the host, usually a reviewing agent, that
//! answers host passthrough requests in place of the user.
//!
//! A sandboxed agent cannot change host configuration. The risk is a process
//! on the host itself, for example an agent that runs without paddock, turning
//! automatic approval on. So the hook is ignored unless its configuration is
//! trusted in one of two ways:
//!
//! 1. System: `/etc/paddock/approver.d/<user>.toml`, owned by root and not
//!    writable by anyone else. Only `sudo` can create or change it.
//! 2. Password: `$XDG_CONFIG_HOME/paddock/approver.toml` with an HMAC whose key
//!    comes from an application password (Argon2id). Without the password,
//!    nobody can produce a valid file. The key exists only while paddock runs:
//!    the user types the password when a sandbox starts.

use crate::broker::{Answer, Approver, Pending, CAPABILITIES};
use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const SYSTEM_DIR: &str = "/etc/paddock/approver.d";

/// What the hook runs and what it may answer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    /// The command, without a shell. It reads one request as JSON on stdin.
    pub command: Vec<String>,
    /// Capabilities the hook may answer. Others go to the user.
    pub capabilities: Vec<String>,
    /// Seconds before the hook counts as abstaining.
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

fn default_timeout() -> u64 {
    60
}

impl HookConfig {
    pub fn validate(&self) -> Result<()> {
        if self.command.is_empty() {
            bail!("the approver needs a command");
        }
        if !Path::new(&self.command[0]).is_absolute() {
            bail!("the approver command must be an absolute path, so PATH cannot change what runs");
        }
        if self.capabilities.is_empty() {
            bail!("name at least one capability that the approver may answer");
        }
        for c in &self.capabilities {
            if !CAPABILITIES.iter().any(|(k, _)| k == c) {
                bail!("unknown capability {c}");
            }
        }
        if !(5..=600).contains(&self.timeout) {
            bail!("timeout must be 5 to 600 seconds");
        }
        Ok(())
    }

    /// The bytes that the password MAC covers: the configuration and the
    /// content of the command file, so editing the script breaks the MAC.
    fn canonical(&self) -> Vec<u8> {
        use sha2::Digest;
        let mut v = serde_json::to_vec(self).unwrap_or_default();
        let file = std::fs::read(&self.command[0]).unwrap_or_default();
        v.extend_from_slice(b"\0command-sha256:");
        v.extend_from_slice(&Sha256::digest(&file));
        v
    }
}

/// The password-protected file in the user configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedConfig {
    #[serde(flatten)]
    pub hook: HookConfig,
    /// Argon2id salt, hex.
    pub salt: String,
    /// HMAC-SHA256 over the hook configuration, hex.
    pub mac: String,
}

pub fn system_file(user: &str) -> PathBuf {
    Path::new(SYSTEM_DIR).join(format!("{user}.toml"))
}

pub fn user_file(env: &HostEnv) -> PathBuf {
    env.paddock_config().join("approver.toml")
}

/// A path is trusted when it and every parent are owned by root and not
/// writable by group or others.
pub fn root_owned_chain(path: &Path) -> Result<()> {
    let mut cur = Some(path);
    while let Some(p) = cur {
        let m =
            std::fs::symlink_metadata(p).with_context(|| format!("cannot read {}", p.display()))?;
        if m.file_type().is_symlink() {
            bail!("{} is a symbolic link", p.display());
        }
        if m.uid() != 0 {
            bail!("{} is not owned by root", p.display());
        }
        if m.mode() & 0o022 != 0 {
            bail!("{} is writable by group or others", p.display());
        }
        cur = p.parent().filter(|q| !q.as_os_str().is_empty());
    }
    Ok(())
}

fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    argon2::Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow::anyhow!("key derivation failed: {e}"))?;
    Ok(key)
}

fn mac_of(key: &[u8; 32], hook: &HookConfig) -> Vec<u8> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("any key length works");
    m.update(&hook.canonical());
    m.finalize().into_bytes().to_vec()
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        bail!("bad hex");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).context("bad hex"))
        .collect()
}

pub fn sign(hook: &HookConfig, password: &str) -> Result<SignedConfig> {
    hook.validate()?;
    let mut salt = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut salt)?;
    let key = derive_key(password, &salt)?;
    Ok(SignedConfig {
        hook: hook.clone(),
        salt: crate::manifest::hex(&salt),
        mac: crate::manifest::hex(&mac_of(&key, hook)),
    })
}

/// Check the MAC in constant time. A wrong password and a changed file look
/// the same: both fail.
pub fn verify(cfg: &SignedConfig, password: &str) -> Result<HookConfig> {
    let key = derive_key(password, &unhex(&cfg.salt)?)?;
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&key).expect("any key length works");
    m.update(&cfg.hook.canonical());
    m.verify_slice(&unhex(&cfg.mac)?).map_err(|_| {
        anyhow::anyhow!("wrong password, or the file was changed after it was signed")
    })?;
    cfg.hook.validate()?;
    Ok(cfg.hook.clone())
}

/// Where a trusted configuration came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    System(PathBuf),
    Password(PathBuf),
}

/// Load the system configuration, if it exists and is trusted.
pub fn load_system(user: &str) -> Result<Option<HookConfig>> {
    let f = system_file(user);
    if !f.exists() {
        return Ok(None);
    }
    root_owned_chain(&f).with_context(|| format!("{} is ignored", f.display()))?;
    let hook: HookConfig = toml::from_str(&std::fs::read_to_string(&f)?)
        .with_context(|| format!("{} is invalid", f.display()))?;
    hook.validate()?;
    // A root-owned file that names a user-writable script protects nothing.
    let cmd = std::fs::canonicalize(&hook.command[0])
        .with_context(|| format!("{} does not exist", hook.command[0]))?;
    root_owned_chain(&cmd).with_context(|| {
        format!(
            "{} is ignored: its command must be owned by root",
            f.display()
        )
    })?;
    Ok(Some(hook))
}

pub fn load_signed(env: &HostEnv) -> Result<Option<SignedConfig>> {
    let f = user_file(env);
    if !f.exists() {
        return Ok(None);
    }
    Ok(Some(
        toml::from_str(&std::fs::read_to_string(&f)?)
            .with_context(|| format!("{} is invalid", f.display()))?,
    ))
}

/// Find a trusted hook for this run. The password file asks for its password
/// on the terminal; Enter skips the hook for this run.
pub fn resolve(env: &HostEnv, interactive: bool) -> Result<Option<(HookConfig, Source)>> {
    match load_system(&env.user) {
        Ok(Some(h)) => return Ok(Some((h, Source::System(system_file(&env.user))))),
        Ok(None) => {}
        Err(e) => log::warn!("{e:#}"),
    }
    let Some(signed) = load_signed(env)? else {
        return Ok(None);
    };
    if !interactive {
        eprintln!("paddock: the agent approver needs its password, and there is no terminal; host requests go to you this time");
        return Ok(None);
    }
    let pw =
        rpassword::prompt_password("paddock: password for the agent approver (Enter to skip): ")?;
    if pw.is_empty() {
        return Ok(None);
    }
    let hook = verify(&signed, &pw)?;
    Ok(Some((hook, Source::Password(user_file(env)))))
}

/// The hook's reply on stdout.
#[derive(Debug, Deserialize)]
struct Reply {
    answer: String,
    #[serde(default)]
    note: String,
}

/// Run the hook for one request. None means abstain: the user decides.
pub fn run_hook(hook: &HookConfig, p: &Pending) -> (Option<Answer>, String) {
    if !hook.capabilities.contains(&p.capability) {
        return (None, "not a capability the approver may answer".into());
    }
    let input = serde_json::json!({ "api_version": "paddock.approval-request/v1", "request": p });
    let child = Command::new(&hook.command[0])
        .args(&hook.command[1..])
        .env("PADDOCK_APPROVAL", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (None, format!("cannot start the approver: {e}")),
    };
    let _ = child
        .stdin
        .take()
        .map(|mut s| s.write_all(input.to_string().as_bytes()));
    let mut out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.by_ref().take(64 * 1024).read_to_end(&mut b);
        b
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < Duration::from_secs(hook.timeout) => {
                std::thread::sleep(Duration::from_millis(50))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return (
                    None,
                    format!("the approver did not answer in {} seconds", hook.timeout),
                );
            }
        }
    }
    let text = String::from_utf8_lossy(&reader.join().unwrap_or_default()).into_owned();
    let Some(line) = text.lines().rev().find(|l| !l.trim().is_empty()) else {
        return (None, "the approver printed nothing".into());
    };
    let reply: Reply = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(_) => {
            return (
                None,
                format!(
                    "the approver printed something other than a JSON answer: {}",
                    line.chars().take(200).collect::<String>()
                ),
            )
        }
    };
    let note: String = reply.note.chars().take(500).collect();
    match reply.answer.as_str() {
        "abstain" => (None, note),
        "session" if !p.session_allowed => (
            None,
            "the approver asked for session, which this request does not allow".into(),
        ),
        a => match a.parse::<Answer>() {
            Ok(ans) => (Some(ans), note),
            Err(_) => (None, format!("unknown answer {a}")),
        },
    }
}

/// The hook first; the user when it abstains, fails or times out.
pub struct Chain {
    pub hook: HookConfig,
    pub source: Source,
    pub human: Arc<dyn Approver>,
}

impl Approver for Chain {
    fn ask(&self, env: &HostEnv, p: &Pending, wait: Duration) -> Option<Answer> {
        self.ask_with_source(env, p, wait).map(|(a, _)| a)
    }

    fn ask_with_source(
        &self,
        env: &HostEnv,
        p: &Pending,
        wait: Duration,
    ) -> Option<(Answer, &'static str)> {
        let (answer, note) = run_hook(&self.hook, p);
        if let Some(a) = answer {
            record(env, p, &format!("{a:?}").to_lowercase(), &note);
            return Some((a, "agent"));
        }
        // The terminal belongs to the harness, so the reason for passing goes
        // to the decision log, not to the screen.
        record(env, p, "passed to the user", &note);
        self.human.ask(env, p, wait).map(|a| (a, "user"))
    }
}

fn record(env: &HostEnv, p: &Pending, a: &str, note: &str) {
    // The request detail can hold clipboard text or command arguments, so it
    // stays out. The request id matches the line in the host audit log.
    let f = env.paddock_state().join("approver-decisions.jsonl");
    if let Ok(mut file) = crate::privfs::append(&f) {
        let _ = writeln!(
            file,
            "{}",
            serde_json::json!({ "time": crate::state::now(), "request": p.id, "capability": p.capability, "answer": a, "note": note })
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn hook(cmd: &Path, caps: &[&str]) -> HookConfig {
        HookConfig {
            command: vec![cmd.display().to_string()],
            capabilities: caps.iter().map(|c| c.to_string()).collect(),
            timeout: 5,
        }
    }

    fn pending(cap: &str) -> Pending {
        Pending {
            id: "r-1-a".into(),
            sandbox: "s".into(),
            harness: "claude".into(),
            project: "/p".into(),
            capability: cap.into(),
            summary: "x".into(),
            detail: "x".into(),
            reason: String::new(),
            created: 1,
            expires: 2,
            session_allowed: true,
            started_by: vec![],
        }
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn signature_detects_any_change_and_a_wrong_password() {
        let h = hook(Path::new("/usr/bin/true"), &["screenshot"]);
        let signed = sign(&h, "correct horse").unwrap();
        assert_eq!(verify(&signed, "correct horse").unwrap(), h);
        assert!(verify(&signed, "wrong").is_err());
        let mut changed = signed.clone();
        changed.hook.capabilities.push("exec".into());
        assert!(
            verify(&changed, "correct horse").is_err(),
            "adding a capability breaks the signature"
        );
        let text = toml::to_string(&signed).unwrap();
        let back: SignedConfig = toml::from_str(&text).unwrap();
        assert!(verify(&back, "correct horse").is_ok());
    }

    #[test]
    fn editing_the_command_file_breaks_the_signature() {
        let tmp = tempfile::tempdir().unwrap();
        let s = script(tmp.path(), "review", "echo '{\"answer\":\"deny\"}'");
        let signed = sign(&hook(&s, &["screenshot"]), "correct horse").unwrap();
        assert!(verify(&signed, "correct horse").is_ok());
        std::fs::write(&s, "#!/bin/sh\necho '{\"answer\":\"once\"}'\n").unwrap();
        assert!(verify(&signed, "correct horse").is_err());
    }

    #[test]
    fn validation_rules() {
        assert!(hook(Path::new("relative"), &["exec"]).validate().is_err());
        assert!(hook(Path::new("/bin/true"), &[]).validate().is_err());
        assert!(hook(Path::new("/bin/true"), &["rootshell"])
            .validate()
            .is_err());
        assert!(hook(Path::new("/bin/true"), &["exec"]).validate().is_ok());
    }

    #[test]
    fn user_owned_files_are_not_trusted_as_system_files() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("u.toml");
        std::fs::write(&f, "").unwrap();
        assert!(root_owned_chain(&f).is_err());
        // A root-owned system path, if this machine has one, passes.
        if Path::new("/etc/passwd").exists() {
            assert!(root_owned_chain(Path::new("/etc/passwd")).is_ok());
        }
    }

    #[test]
    fn hook_answers_abstains_and_fails_safely() {
        let tmp = tempfile::tempdir().unwrap();
        let allow = script(
            tmp.path(),
            "allow",
            r#"cat >/dev/null; echo '{"answer":"once","note":"looks fine"}'"#,
        );
        let (a, note) = run_hook(&hook(&allow, &["screenshot"]), &pending("screenshot"));
        assert_eq!(a, Some(Answer::Once));
        assert_eq!(note, "looks fine");
        // A capability outside its list goes to the user without running it.
        assert_eq!(
            run_hook(&hook(&allow, &["screenshot"]), &pending("exec")).0,
            None
        );

        let abstain = script(
            tmp.path(),
            "abstain",
            r#"cat >/dev/null; echo '{"answer":"abstain"}'"#,
        );
        assert_eq!(
            run_hook(&hook(&abstain, &["exec"]), &pending("exec")).0,
            None
        );
        let garbage = script(tmp.path(), "garbage", "cat >/dev/null; echo yes");
        assert_eq!(
            run_hook(&hook(&garbage, &["exec"]), &pending("exec")).0,
            None
        );
        let crash = script(tmp.path(), "crash", "exit 1");
        assert_eq!(run_hook(&hook(&crash, &["exec"]), &pending("exec")).0, None);
        let mut slow = hook(&script(tmp.path(), "slow", "sleep 30"), &["exec"]);
        slow.timeout = 5;
        let start = Instant::now();
        assert_eq!(run_hook(&slow, &pending("exec")).0, None);
        assert!(start.elapsed() < Duration::from_secs(10));
        let mut no_session = pending("exec");
        no_session.session_allowed = false;
        let session = script(
            tmp.path(),
            "session",
            r#"cat >/dev/null; echo '{"answer":"session"}'"#,
        );
        assert_eq!(run_hook(&hook(&session, &["exec"]), &no_session).0, None);
    }

    #[test]
    fn the_hook_receives_the_request_on_stdin() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("seen.json");
        let spy = script(
            tmp.path(),
            "spy",
            &format!(r#"cat > '{}'; echo '{{"answer":"deny"}}'"#, out.display()),
        );
        assert_eq!(
            run_hook(&hook(&spy, &["fetch"]), &pending("fetch")).0,
            Some(Answer::Deny)
        );
        let seen: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out).unwrap()).unwrap();
        assert_eq!(seen["api_version"], "paddock.approval-request/v1");
        assert_eq!(seen["request"]["capability"], "fetch");
    }
}
