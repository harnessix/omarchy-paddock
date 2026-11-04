//! The VM backend on macOS: the run starts with Linux paddock and bwrap in
//! a light Linux VM. The broker, the proxy and the logins of the host run
//! stay on the host. The launch is `paddock vm-attach JOB`, which starts
//! the VM when none runs and connects the terminal to the run in the guest.

use super::{Backend, Launch};
use crate::env::HostEnv;
use crate::spec::{GuiMode, Network, SandboxSpec};
use crate::vm::{assets, host, proto, select};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

pub use select::NAME;

pub struct Vm;

/// The token of each run of this process, by sandbox id.
static TOKENS: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

/// The secret token of a run. It names the run to the guest agent and in
/// the route folders.
pub fn token(id: &str) -> Result<String> {
    let mut t = TOKENS.lock().unwrap();
    if let Some(tok) = t.get(id) {
        return Ok(tok.clone());
    }
    let tok = proto::new_token()?;
    t.insert(id.to_string(), tok.clone());
    Ok(tok)
}

/// The login callback forwarders of the runs of this process.
static CALLBACKS: Mutex<Vec<crate::vm::attach::Callbacks>> = Mutex::new(Vec::new());

/// The job file of `paddock vm-attach`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Job {
    pub request: proto::RunRequest,
    /// The broker socket of the run on the host.
    pub broker: PathBuf,
    /// The proxy socket of the run on the host, in network modes egress
    /// and api.
    pub proxy: Option<PathBuf>,
}

/// The host proxy socket of a run.
pub fn proxy_socket(env: &HostEnv, id: &str) -> Result<PathBuf> {
    crate::sockpath::named(env, id, "vm-proxy.sock")
}

/// Spec features that the VM backend cannot give.
pub fn refuse(spec: &SandboxSpec) -> Result<()> {
    match spec.network {
        Network::Host => bail!("network mode 'host' needs the host network, and the VM has no network device: use --network egress"),
        Network::Allowlist => bail!("network mode 'allowlist' needs the srt backend: on macOS use --network api with --allow"),
        _ => {}
    }
    match spec.gui.mode {
        GuiMode::Off => {}
        GuiMode::App => bail!("app mode runs desktop apps with Seatbelt: use --backend seatbelt"),
        m => bail!(
            "display mode '{}' needs a display in the VM, which it does not have: use --gui off",
            format!("{m:?}").to_lowercase()
        ),
    }
    Ok(())
}

/// Start paddock's proxy for the run: the guest reaches it over the proxy
/// port. Network mode api gets the allowlist of the run.
pub fn start_proxy(env: &HostEnv, spec: &SandboxSpec) -> Result<()> {
    // Login callbacks: the ports that the host broker allows go into the
    // guest, as long as this process runs.
    CALLBACKS
        .lock()
        .unwrap()
        .push(crate::vm::attach::Callbacks::start(env, token(&spec.id)?));
    let mut cfg = super::host_proxy_cfg(env, spec);
    match spec.network {
        Network::Egress => {}
        Network::Api => {
            let (list, _) = crate::proxy::allow::Allowlist::parse(&spec.allow_domains);
            cfg.allow_loopback = Vec::new();
            cfg.api = Some(crate::proxy::allow::ApiPolicy {
                list,
                refusals: Some(super::api_refusals(env, spec)),
            });
        }
        _ => return Ok(()),
    }
    crate::proxy::serve(&proxy_socket(env, &spec.id)?, cfg)
}

/// The run request for the guest.
pub fn request(env: &HostEnv, spec: &SandboxSpec, token: String) -> proto::RunRequest {
    let mut vars = BTreeMap::new();
    for (k, v) in &env.vars {
        if proto::allowed_var(k) {
            vars.insert(k.clone(), v.clone());
        }
    }
    proto::RunRequest {
        v: proto::VERSION,
        token,
        id: spec.id.clone(),
        harness: spec.harness.clone(),
        project: spec.project.clone(),
        network: match spec.network {
            Network::Egress | Network::Api => proto::GuestNet::Proxy,
            _ => proto::GuestNet::None,
        },
        args: spec.intent.args.clone(),
        tty: false,
        rows: 0,
        cols: 0,
        env: vars,
        shell: spec.intent.shell,
        no_yolo: spec.intent.no_yolo,
        profile: user_profile(env, &spec.harness),
        owner: owner(&spec.project),
    }
}

/// The uid that owns the project folder on the host.
fn owner(project: &std::path::Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(project).ok().map(|m| m.uid())
}

/// The text of the user's own profile of `harness`: the guest has only the
/// built-in profiles.
fn user_profile(env: &HostEnv, harness: &str) -> Option<String> {
    let file = crate::profile::HarnessFactory::new(env)
        .ok()?
        .user_file(harness)?;
    match std::fs::read_to_string(&file) {
        Ok(t) => Some(t),
        Err(e) => {
            log::warn!("cannot read the profile {}: {e}", file.display());
            None
        }
    }
}

impl Backend for Vm {
    fn name(&self) -> &'static str {
        NAME
    }

    fn check(&self, env: &HostEnv, spec: &SandboxSpec) -> Result<()> {
        if !cfg!(target_os = "macos") {
            bail!(
                "the vm backend is for macOS. On Linux, paddock runs the sandbox itself: use bwrap"
            );
        }
        refuse(spec)?;
        if let Err(e) = assets::check(&assets::assets_dir(env)) {
            bail!("the VM files are not set up ({e:#}). Run `paddock vm setup`, then start again");
        }
        host::helper_path(env)?;
        Ok(())
    }

    fn render(&self, env: &HostEnv, spec: &SandboxSpec, interactive: bool) -> Result<Launch> {
        let broker = spec
            .host_socket
            .clone()
            .context("the VM backend needs the host broker of the run")?;
        let proxy = matches!(spec.network, Network::Egress | Network::Api)
            .then(|| proxy_socket(env, &spec.id))
            .transpose()?;
        let mut req = request(env, spec, token(&spec.id)?);
        req.tty = interactive;
        let job = Job {
            request: req,
            broker,
            proxy,
        };
        let file = env.paddock_run().join(&spec.id).join("vm-job.json");
        let me = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .context("cannot find the paddock executable")?;
        let mut vars = BTreeMap::new();
        for k in [
            "HOME",
            "USER",
            "LOGNAME",
            "PATH",
            "TMPDIR",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
            "XDG_RUNTIME_DIR",
            "PADDOCK_LOG",
        ] {
            if let Some(v) = env.var(k) {
                vars.insert(k.to_string(), v.to_string());
            }
        }
        vars.insert("HOME".into(), env.home.display().to_string());
        Ok(Launch {
            program: me,
            args: vec!["vm-attach".into(), file.display().to_string()],
            env: vars,
            files: vec![(file, serde_json::to_string(&job)?, 0o600)],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(t: &std::path::Path) -> (HostEnv, SandboxSpec) {
        let mut vars = std::collections::HashMap::new();
        vars.insert("TERM".to_string(), "xterm".to_string());
        vars.insert("AWS_SECRET_ACCESS_KEY".to_string(), "x".to_string());
        let env = HostEnv::from_parts(t.into(), "u".into(), 501, 20, vars.into_iter().collect());
        let mut s = SandboxSpec::new(
            "fake-p-1".into(),
            "fake",
            &t.join("p"),
            t.join("sb"),
            t,
            501,
            20,
            "u",
        );
        s.intent.args = vec!["--x".into()];
        (env, s)
    }

    #[test]
    fn the_request_carries_the_intent_and_only_terminal_variables() {
        let t = tempfile::tempdir().unwrap();
        let (env, mut s) = spec(t.path());
        s.network = Network::Api;
        let r = request(&env, &s, "a".repeat(32));
        assert!(r.check().is_ok());
        assert_eq!(r.harness, "fake");
        assert_eq!(r.args, ["--x"]);
        assert_eq!(r.network, proto::GuestNet::Proxy);
        assert_eq!(r.env.get("TERM").map(String::as_str), Some("xterm"));
        assert!(!r.env.contains_key("AWS_SECRET_ACCESS_KEY"));
        // A built-in profile stays in the guest. A user profile goes along.
        assert_eq!(r.profile, None);
        let dir = env.paddock_config().join("harness.d");
        std::fs::create_dir_all(&dir).unwrap();
        let text = "name = \"fake\"\n[locate]\ncommands = [\"sh\"]\n";
        std::fs::write(dir.join("fake.toml"), text).unwrap();
        let r = request(&env, &s, "a".repeat(32));
        assert_eq!(r.profile.as_deref(), Some(text));
        s.network = Network::None;
        assert_eq!(
            request(&env, &s, "a".repeat(32)).network,
            proto::GuestNet::None
        );
    }

    #[test]
    fn the_vm_refuses_host_network_and_a_display() {
        let t = tempfile::tempdir().unwrap();
        let (_, mut s) = spec(t.path());
        assert!(refuse(&s).is_ok());
        s.network = Network::Host;
        assert!(refuse(&s)
            .unwrap_err()
            .to_string()
            .contains("no network device"));
        s.network = Network::Egress;
        s.gui.mode = GuiMode::Window;
        assert!(refuse(&s).unwrap_err().to_string().contains("--gui off"));
    }

    #[test]
    fn a_token_stays_the_same_for_a_run() {
        let a = token("run-a").unwrap();
        assert_eq!(token("run-a").unwrap(), a);
        assert_ne!(token("run-b").unwrap(), a);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_vm_backend_is_for_macos_only() {
        let t = tempfile::tempdir().unwrap();
        let (env, s) = spec(t.path());
        let e = Vm.check(&env, &s).unwrap_err().to_string();
        assert!(e.contains("for macOS"), "{e}");
    }
}
