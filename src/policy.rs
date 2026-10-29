//! The policy engine runs last. It never adds access. It refuses a spec that
//! breaks an invariant and names the source of the offending entry.

use crate::detect::Git;
use crate::env::HostEnv;
use crate::spec::{MountKind, SandboxSpec};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Paths under HOME that hold credentials or private data. They are never
/// mounted, and no mount may contain them.
pub const DENIED_HOME: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".password-store",
    ".netrc",
    ".git-credentials",
    ".gitconfig",
    ".config/git",
    ".config/gh",
    ".config/hub",
    ".docker",
    ".kube",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".oci",
    ".terraform.d",
    ".config/rclone",
    ".local/share/keyrings",
    ".pki",
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
    ".config/vivaldi",
    ".thunderbird",
    ".cargo/credentials.toml",
    ".cargo/credentials",
    ".npmrc",
    ".pypirc",
    ".config/op",
    ".config/Bitwarden",
];

/// Host sockets that give control over the host.
pub const DENIED_ABS: &[&str] = &[
    "/var/run/docker.sock",
    "/run/docker.sock",
    "/run/containerd",
    "/run/podman",
    "/var/run/libvirt",
    "/run/dbus",
    "/proc",
    "/sys",
    "/dev",
    "/boot",
    "/root",
];

/// Host variables that are never inherited: the sandbox sets them itself, or
/// they name a socket or control channel of the host session.
pub const HOST_ONLY_ENV: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "PATH",
    "SHELL",
    "PWD",
    "OLDPWD",
    "SHLVL",
    "_",
    "TMPDIR",
    "TMP",
    "TEMP",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_ID",
    "XDG_SEAT",
    "XDG_VTNR",
    "XDG_SESSION_CLASS",
    "XDG_SESSION_TYPE",
    "DBUS_SESSION_BUS_ADDRESS",
    "DBUS_SYSTEM_BUS_ADDRESS",
    "WAYLAND_DISPLAY",
    "WAYLAND_SOCKET",
    "DISPLAY",
    "XAUTHORITY",
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "SSH_CONNECTION",
    "SSH_CLIENT",
    "SSH_TTY",
    "GPG_AGENT_INFO",
    "GNOME_KEYRING_CONTROL",
    "SESSION_MANAGER",
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "CONTAINER_HOST",
    "PODMAN_HOST",
    "KUBECONFIG",
    "HYPRLAND_INSTANCE_SIGNATURE",
    "HYPRLAND_CMD",
    "SWAYSOCK",
    "I3SOCK",
    "NIRI_SOCKET",
    "KITTY_LISTEN_ON",
    "KITTY_PID",
    "KITTY_WINDOW_ID",
    "TMUX",
    "TMUX_PANE",
    "ZELLIJ",
    "ZELLIJ_SESSION_NAME",
    "STY",
    "WEZTERM_UNIX_SOCKET",
    "NVIM",
    "NVIM_LISTEN_ADDRESS",
    "VSCODE_IPC_HOOK_CLI",
    "VSCODE_GIT_IPC_HANDLE",
    "MANAGERPID",
    "INVOCATION_ID",
    "JOURNAL_STREAM",
    "SYSTEMD_EXEC_PID",
    "NOTIFY_SOCKET",
    "LISTEN_FDS",
    "LISTEN_PID",
    "MISE_SHELL",
    "LD_PRELOAD",
    "LD_AUDIT",
    "PADDOCK",
    "PADDOCK_ID",
    "PADDOCK_HARNESS",
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
];

/// Whether a host variable can be inherited. Besides the fixed list, any
/// variable whose value is the path of a socket, or whose name says it holds
/// one, is a host control channel and stays outside.
pub fn inheritable_var(name: &str, value: &str) -> bool {
    if !inheritable(name) {
        return false;
    }
    let upper = name.to_ascii_uppercase();
    if upper.ends_with("_SOCK")
        || upper.ends_with("_SOCKET")
        || upper.contains("_IPC_")
        || upper.ends_with("_IPC")
    {
        return false;
    }
    let path = value.strip_prefix("unix:").unwrap_or(value);
    if path.starts_with('/') {
        use std::os::unix::fs::FileTypeExt;
        if std::fs::metadata(path)
            .map(|m| m.file_type().is_socket())
            .unwrap_or(false)
        {
            return false;
        }
    }
    true
}

/// High-privilege cloud and infrastructure credentials. Inheritance drops
/// them by default: they can create, delete or bill resources far beyond the
/// project. `[env] pass` or a profile's `env_pass` lets one through by name.
pub const CLOUD_SECRETS: &[&str] = &[
    // AWS
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_SECURITY_TOKEN",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN",
    // Azure
    "AZURE_CLIENT_SECRET",
    "AZURE_CLIENT_CERTIFICATE_PATH",
    "AZURE_CLIENT_CERTIFICATE_PASSWORD",
    "AZURE_PASSWORD",
    "AZURE_STORAGE_KEY",
    "AZURE_STORAGE_CONNECTION_STRING",
    "ARM_CLIENT_SECRET",
    "ARM_ACCESS_KEY",
    // Google Cloud
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CREDENTIALS",
    "GOOGLE_OAUTH_ACCESS_TOKEN",
    "CLOUDSDK_AUTH_ACCESS_TOKEN",
    "GOOGLE_GHA_CREDS_PATH",
    // Other clouds and CDNs
    "DIGITALOCEAN_ACCESS_TOKEN",
    "DIGITALOCEAN_TOKEN",
    "HCLOUD_TOKEN",
    "LINODE_TOKEN",
    "VULTR_API_KEY",
    "CLOUDFLARE_API_TOKEN",
    "CLOUDFLARE_API_KEY",
    "CF_API_TOKEN",
    "CF_API_KEY",
    "IBMCLOUD_API_KEY",
    "ALIBABA_CLOUD_ACCESS_KEY_ID",
    "ALIBABA_CLOUD_ACCESS_KEY_SECRET",
    "ALIBABA_CLOUD_SECURITY_TOKEN",
    "ALICLOUD_SECRET_KEY",
    "TENCENTCLOUD_SECRET_KEY",
    "HUAWEICLOUD_SDK_SK",
    "OCI_CLI_KEY_FILE",
    "SCW_SECRET_KEY",
    "OS_PASSWORD",
    // Infrastructure control planes
    "VAULT_TOKEN",
    "NOMAD_TOKEN",
    "CONSUL_HTTP_TOKEN",
    "TFE_TOKEN",
    "HCP_CLIENT_SECRET",
    "PULUMI_ACCESS_TOKEN",
];

pub fn is_cloud_secret(name: &str) -> bool {
    CLOUD_SECRETS.contains(&name) || name.starts_with("TF_TOKEN_")
}

pub fn inheritable(name: &str) -> bool {
    !HOST_ONLY_ENV.contains(&name)
        && !name.starts_with("__MISE")
        && !name.starts_with("PADDOCK_")
        && !name.starts_with("BASH_FUNC_")
        && !name.contains(['=', '\0'])
}

/// Environment names that look like secrets. They pass only when a profile or
/// manifest names them explicitly.
pub fn looks_secret(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "API_KEY",
        "APIKEY",
        "CREDENTIAL",
        "PRIVATE_KEY",
    ]
    .iter()
    .any(|w| n.contains(w))
}

pub fn denied_paths(env: &HostEnv) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = DENIED_HOME.iter().map(|p| env.home.join(p)).collect();
    v.extend(DENIED_ABS.iter().map(PathBuf::from));
    if let Some(rt) = &env.runtime_dir {
        // The whole runtime directory holds the session bus, keyrings and the
        // display sockets. Only the Wayland socket may pass, and only for GUI.
        v.push(rt.clone());
    }
    v
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

pub fn enforce(env: &HostEnv, spec: &SandboxSpec) -> Result<()> {
    let denied = denied_paths(env);
    // The display socket is not a mount entry: gui.rs sets it only for the
    // nested compositor (window mode) or the approved host mode.

    for m in &spec.mounts {
        let Some(src) = &m.src else { continue };
        let real = std::fs::canonicalize(src).unwrap_or_else(|_| src.clone());
        for cand in [src, &real] {
            if cand == Path::new("/") {
                bail!("policy: refusing to mount / (from {})", m.origin);
            }
            if env.home.starts_with(cand) {
                bail!(
                    "policy: refusing to mount {}: it is the home directory or contains it (from {})",
                    cand.display(),
                    m.origin
                );
            }
            for d in &denied {
                if overlaps(cand, d) {
                    bail!(
                        "policy: refusing to mount {}: it overlaps the protected path {} (from {})",
                        cand.display(),
                        d.display(),
                        m.origin
                    );
                }
            }
        }
    }

    // Every writable mount that contains a git directory must also carry the
    // read-only overrides for its hooks and config.
    for m in spec.mounts.iter().filter(|m| m.kind == MountKind::Rw) {
        let src = m.src.as_ref().unwrap();
        for git_dir in git_dirs_under(src) {
            let common = common_dir(&git_dir).unwrap_or_else(|| git_dir.clone());
            for dir in Git::protected_dirs(&git_dir, &common) {
                for p in [dir.join("hooks"), dir.join("config")] {
                    if !p.exists() || !p.starts_with(src) {
                        continue;
                    }
                    let covered = spec
                        .mounts
                        .iter()
                        .any(|o| o.dst == p && o.kind == MountKind::Ro);
                    if !covered {
                        bail!(
                            "policy: {} is writable inside {} (from {}). Git runs it on the host.",
                            p.display(),
                            src.display(),
                            m.origin
                        );
                    }
                }
            }
        }
    }

    for (k, v) in &spec.env {
        let explicit = ["profile", "manifest", "host env", "network"]
            .iter()
            .any(|o| v.origin.starts_with(o));
        if looks_secret(k) && !explicit {
            bail!("policy: environment variable {k} looks like a secret and was not requested explicitly (from {})", v.origin);
        }
    }
    Ok(())
}

fn git_dirs_under(root: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    let top = root.join(".git");
    if top.is_dir() {
        v.push(top);
    }
    if root.join("HEAD").is_file() && root.join("objects").is_dir() {
        v.push(root.to_path_buf());
    }
    v
}

fn common_dir(git_dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Mount;
    use std::collections::BTreeMap;

    fn fake_env(home: &Path) -> HostEnv {
        HostEnv::from_parts(
            home.to_path_buf(),
            "tester".into(),
            4242,
            4242,
            BTreeMap::new(),
        )
    }

    fn spec(env: &HostEnv, project: &Path) -> SandboxSpec {
        SandboxSpec::new(
            "t".into(),
            "x",
            project,
            env.home.join("sandbox"),
            &env.home,
            1,
            1,
            "tester",
        )
    }

    #[test]
    fn refuses_home_and_denied_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_env(tmp.path());
        let project = tmp.path().join("p");
        std::fs::create_dir_all(&project).unwrap();

        let mut s = spec(&env, &project);
        s.add_mount(Mount::same(tmp.path(), false, "test"));
        assert!(enforce(&env, &s).is_err());

        let mut s = spec(&env, &project);
        s.add_mount(Mount::same(tmp.path().join(".config"), false, "test"));
        assert!(enforce(&env, &s)
            .unwrap_err()
            .to_string()
            .contains("protected path"));

        let mut s = spec(&env, &project);
        s.add_mount(Mount::same("/var/run/docker.sock", true, "test"));
        assert!(enforce(&env, &s).is_err());

        let mut s = spec(&env, &project);
        s.add_mount(Mount::same(&project, true, "test"));
        assert!(enforce(&env, &s).is_ok());
    }

    #[test]
    fn refuses_writable_git_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_env(tmp.path());
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        let ok = Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(&project)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let mut s = spec(&env, &project);
        s.add_mount(Mount::same(&project, true, "test project"));
        assert!(enforce(&env, &s).unwrap_err().to_string().contains("hooks"));
    }

    #[test]
    fn refuses_unrequested_secret_env() {
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_env(tmp.path());
        let mut s = spec(&env, tmp.path());
        s.set_env("GITHUB_TOKEN", "x", "base");
        assert!(enforce(&env, &s).is_err());
        s.set_env("GITHUB_TOKEN", "x", "profile copilot env_pass");
        assert!(enforce(&env, &s).is_ok());
    }
}
