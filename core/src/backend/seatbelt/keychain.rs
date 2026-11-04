//! A keychain of its own for each login of a Seatbelt terminal run.
//!
//! On macOS, harnesses keep logins in the Keychain: Claude Code runs
//! `security add-generic-password`, and Codex keeps MCP tokens there with
//! the `keyring` crate. Both use the file-based keychain. paddock gives each
//! login (harness and auth mode) a keychain file in its login store, with a
//! random password in a file next to it, and unlocks it before the run.
//!
//! How the sandbox finds it: the Security framework reads the keychain
//! search list from `$HOME/Library/Preferences/com.apple.security.plist`
//! (`getenv("HOME")`, not `CFFIXED_USER_HOME`). HOME is the sandbox home, so
//! paddock writes that file there, with this keychain as the only entry and
//! as the default keychain.
//!
//! Why the sandbox cannot reach the user's login keychain: the client
//! process reads the keychain file itself and sends its blobs to securityd
//! (`com.apple.SecurityServer`). Seatbelt denies the real home, so the file
//! of the login keychain stays closed, whatever the search list says. The
//! system keychains in `/Library/Keychains` are denied too. The data
//! protection keychain (`com.apple.securityd.xpc`) cannot be moved to a file
//! of paddock's: secd picks the keychain of the user. Its service stays
//! denied, as do the other keychain services.

use crate::env::HostEnv;
use crate::seatbelt::{Access, Policy};
use crate::spec::SandboxSpec;
use crate::state::AuthMode;
use anyhow::{bail, Context as _, Result};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The keychain file of the run, as a host path. The Seatbelt profile
/// allows its folder.
pub const VAR: &str = "PADDOCK_KEYCHAIN";
/// The keychain file of a login. Not `login.keychain-db`: macOS's security
/// service treats a keychain whose name holds `login` as a login keychain,
/// and unlocking it with paddock's own password fails.
pub const FILE: &str = "paddock.keychain-db";
/// The search list in the sandbox home.
pub const SEARCH_LIST: &str = "Library/Preferences/com.apple.security.plist";
/// securityd, which holds the keys of an unlocked file-based keychain.
pub const SECURITY_SERVER: &str = "com.apple.SecurityServer";
/// The Module Directory Services files that the Security framework reads.
pub const MDS: &str = "/private/var/db/mds";
/// The system keychains. Their items ask the user for an admin password, so
/// the sandbox must not reach them.
pub const SYSTEM_KEYCHAINS: &str = "/Library/Keychains";
/// The Apple CSP/DL module of a file-based keychain (`gGuidAppleCSPDL`), and
/// its service type (CSSM_SERVICE_CSP | CSSM_SERVICE_DL).
const CSPDL_GUID: &str = "{87191ca3-0fc9-11d4-849a-000502b52122}";
const CSPDL_TYPE: u32 = 6;

/// Where paddock keeps the keychain of one login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    /// The folder that the sandbox can write: the keychain file and the lock
    /// and temporary files of the Security framework.
    pub dir: PathBuf,
    pub file: PathBuf,
    /// The password, outside `dir`: the sandbox never reads it.
    pub password: PathBuf,
}

/// The keychain of a login. An isolated login keeps it in its own login
/// store, so it goes away with the login. The shared login and the account
/// logins keep it in paddock's data, never in the user's home.
pub fn store(env: &HostEnv, harness: &str, mode: &AuthMode) -> Store {
    let root = match mode {
        AuthMode::Isolated(_) => crate::state::auth_root(env, harness, mode),
        AuthMode::Shared => keychains(env).join(harness).join("shared"),
        AuthMode::Account { key, .. } => {
            keychains(env).join(harness).join(format!("account-{key}"))
        }
    };
    Store {
        dir: root.join("keychain"),
        file: root.join("keychain").join(FILE),
        password: root.join("keychain.password"),
    }
}

fn keychains(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("keychains")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The search list for the sandbox home: `keychain` is the only keychain
/// of the user domain, the default one and the login one. The format is the
/// one that `security list-keychains -d user -s` writes.
pub fn search_list(keychain: &Path) -> Result<String> {
    let Some(name) = keychain.to_str() else {
        bail!("{} is not valid UTF-8", keychain.display());
    };
    if !name.starts_with('/') || name.chars().any(char::is_control) {
        bail!("{name:?} must be an absolute path without control characters");
    }
    let entry = format!(
        "\t\t<dict>\n\t\t\t<key>DbName</key>\n\t\t\t<string>{}</string>\n\t\t\t<key>GUID</key>\n\t\t\t<string>{CSPDL_GUID}</string>\n\t\t\t<key>SubserviceType</key>\n\t\t\t<integer>{CSPDL_TYPE}</integer>\n\t\t</dict>\n",
        xml_escape(name)
    );
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
    );
    for key in ["DLDBSearchList", "DefaultKeychain", "LoginKeychain"] {
        out.push_str(&format!(
            "\t<key>{key}</key>\n\t<array>\n{entry}\t</array>\n"
        ));
    }
    out.push_str("</dict>\n</plist>\n");
    Ok(out)
}

/// The `security` commands that make a new keychain at `file` and unlock
/// it. `new` adds the creation and the lock settings: no lock after a time
/// and no lock on sleep, because the sandbox cannot unlock it again. The
/// password is an argument: only processes of the same user can read it,
/// and they can read the password file too. The sandbox sees its own
/// processes only.
pub fn commands(file: &Path, password: &str, new: bool) -> Vec<Vec<String>> {
    let f = file.display().to_string();
    let mut v = Vec::new();
    if new {
        v.push(vec![
            "create-keychain".into(),
            "-p".into(),
            password.into(),
            f.clone(),
        ]);
        v.push(vec!["set-keychain-settings".into(), f.clone()]);
    }
    v.push(vec![
        "unlock-keychain".into(),
        "-p".into(),
        password.into(),
        f,
    ]);
    v
}

/// The Seatbelt rules for the keychain at `file`: its folder, securityd,
/// and the files that the Security framework reads. The system keychains
/// stay denied.
pub fn allow(p: &mut Policy, file: &Path, user_cache: Option<&Path>) {
    let dir = file.parent().unwrap_or(file);
    p.add(
        super::real(dir),
        Access::ReadWrite,
        "the keychain of this login (in paddock's login store)",
    );
    p.services.push((
        SECURITY_SERVER.into(),
        "securityd: the keys of the unlocked keychain of this login. The keychain files of the user stay denied".into(),
    ));
    p.add(
        MDS,
        Access::ReadOnly,
        "the Security framework's module directory",
    );
    if let Some(c) = user_cache {
        p.add(
            super::real(&c.join("mds")),
            Access::ReadOnly,
            "the Security framework's module directory of this user",
        );
    }
    p.add(
        SYSTEM_KEYCHAINS,
        Access::Deny,
        "the system keychains: their items ask the user for an admin password",
    );
}

/// The user's cache folder (`DARWIN_USER_CACHE_DIR`).
#[cfg(target_os = "macos")]
pub fn user_cache() -> Option<PathBuf> {
    let mut buf = vec![0u8; 1024];
    // SAFETY: confstr writes at most buf.len() bytes, NUL included.
    let n = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_CACHE_DIR,
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if n == 0 || n > buf.len() {
        return None;
    }
    buf.truncate(n - 1);
    String::from_utf8(buf).ok().map(PathBuf::from)
}

#[cfg(not(target_os = "macos"))]
pub fn user_cache() -> Option<PathBuf> {
    None
}

/// A folder or file that paddock uses must not be a symlink: the sandbox
/// can write the keychain folder, and a link could send paddock elsewhere.
fn no_link(p: &Path, dir: bool) -> Result<bool> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => bail!("{} is a symlink", p.display()),
        Ok(m) if m.is_dir() != dir => bail!("{} has the wrong file type", p.display()),
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// The password of a store: read, or made the first time.
fn password(store: &Store) -> Result<String> {
    if no_link(&store.password, false)? {
        let s = std::fs::read_to_string(&store.password)
            .with_context(|| format!("cannot read {}", store.password.display()))?;
        return Ok(s.trim().to_string());
    }
    let mut bytes = [0u8; 32];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
    let pw: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    match crate::privfs::write_new(&store.password, &pw) {
        Ok(()) => Ok(pw),
        // Another start of this login was first.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => password(store),
        Err(e) => Err(e).with_context(|| format!("cannot write {}", store.password.display())),
    }
}

/// Run `security` with a private HOME, so it never changes the search list
/// of the user.
fn security(args: &[String], home: &Path) -> Result<()> {
    let out = std::process::Command::new("/usr/bin/security")
        .args(args)
        .env("HOME", home)
        .env("CFFIXED_USER_HOME", home)
        .stdin(std::process::Stdio::null())
        .output()
        .context("cannot run /usr/bin/security")?;
    if !out.status.success() {
        bail!(
            "security {} failed: {}",
            args[0],
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// A private folder for `security`'s HOME and a new keychain, removed at
/// the end.
struct Scratch(PathBuf);

impl Scratch {
    fn new(root: &Path) -> Result<Self> {
        let mut bytes = [0u8; 8];
        std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
        let name: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let p = root.join(format!(".keychain-{name}"));
        std::fs::DirBuilder::new().mode(0o700).create(&p)?;
        Ok(Self(p))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Make the keychain of the login if it is missing, unlock it, and point
/// the sandbox home's search list at it. A new keychain is made in a
/// private folder and then moved in, so a link that the sandbox planted is
/// replaced, never followed.
pub fn start(env: &HostEnv, spec: &mut SandboxSpec, mode: &AuthMode) -> Result<()> {
    let store = store(env, &spec.harness, mode);
    let root = store.dir.parent().unwrap_or(&store.dir);
    crate::privfs::dir(root)?;
    crate::privfs::dir(&store.dir)?;
    no_link(&store.dir, true)?;
    let pw = password(&store)?;
    let scratch = Scratch::new(root)?;
    let file = super::real(&store.dir).join(FILE);
    if !no_link(&file, false)? {
        let tmp = scratch.0.join(FILE);
        for c in commands(&tmp, &pw, true) {
            security(&c, &scratch.0)?;
        }
        std::fs::rename(&tmp, &file)
            .with_context(|| format!("cannot move the keychain to {}", file.display()))?;
    }
    // `security` makes the file 0644. Only the user reads it.
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot make {} private", file.display()))?;
    for c in commands(&file, &pw, false) {
        security(&c, &scratch.0).with_context(|| {
            format!(
                "the keychain {} does not open with its password in {}",
                file.display(),
                store.password.display()
            )
        })?;
    }
    crate::safefs::Root::open(&spec.home_src)?.write(
        Path::new(SEARCH_LIST),
        search_list(&file)?.as_bytes(),
        0o600,
    )?;
    spec.set_env(
        VAR,
        file.display().to_string(),
        "seatbelt: the keychain of this login",
    );
    Ok(())
}

#[cfg(test)]
#[path = "keychain_tests.rs"]
mod tests;
