//! `paddock update`: replace the paddock binary with the latest GitHub
//! Release, and the update notice in `paddock doctor`.
//!
//! - The release comes from the GitHub API, or from the redirect of the
//!   `releases/latest` page when the API rate limit is used up.
//! - Every request is HTTPS through the same client stack as the egress
//!   proxy: rustls with the system trust store, and the host `HTTPS_PROXY`,
//!   `ALL_PROXY` and `NO_PROXY` (proxy.rs). No new HTTP crate.
//! - A redirect is followed only to the GitHub hosts in [`ALLOWED_HOSTS`].
//! - The tarball and its `.sha256` go into a private 0700 folder. The
//!   checksum must match, and the binary in the tarball must print the
//!   expected `--version`, before anything is replaced.
//! - The new binary is written next to the old one with mode 0755, synced,
//!   and renamed over it. A binary that a pacman package owns is never
//!   replaced: the package manager owns that file.
//! - The command refuses to run inside a sandbox (`PADDOCK=1`).

use crate::env::HostEnv;
use crate::proxy::HostProxy;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use url::Url;

pub const REPO: &str = "harnessix/omarchy-paddock";
pub const REPO_URL: &str = "https://github.com/harnessix/omarchy-paddock";
pub const PLUGIN_ID: &str = "harnessix.paddock";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// The only hosts that paddock talks to for an update. github.com sends
/// release downloads to release-assets.githubusercontent.com (and, for
/// older assets, objects.githubusercontent.com).
pub const ALLOWED_HOSTS: &[&str] = &[
    "api.github.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

const MAX_REDIRECTS: usize = 5;
const MAX_JSON: usize = 2 << 20;
const MAX_DOWNLOAD: usize = 200 << 20;
const MAX_HEAD: usize = 64 << 10;

// ---------------------------------------------------------------------------
// Versions

/// A release version: `MAJOR.MINOR.PATCH` with an optional `-PRE` part, as
/// in semantic versioning. A leading `v` (the tag) is accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
}

impl Version {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        if let Some(p) = pre {
            if p.is_empty()
                || p.split('.').any(|i| i.is_empty())
                || !p
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            {
                return None;
            }
        }
        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() != 3 {
            return None;
        }
        let num = |p: &str| -> Option<u64> {
            if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) || p.len() > 18 {
                return None;
            }
            p.parse().ok()
        };
        Some(Self {
            major: num(parts[0])?,
            minor: num(parts[1])?,
            patch: num(parts[2])?,
            pre: pre.map(str::to_string),
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(p) = &self.pre {
            write!(f, "-{p}")?;
        }
        Ok(())
    }
}

fn cmp_pre(a: &str, b: &str) -> Ordering {
    let mut x = a.split('.');
    let mut y = b.split('.');
    loop {
        match (x.next(), y.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(i), Some(j)) => {
                let o = match (i.parse::<u64>(), j.parse::<u64>()) {
                    (Ok(m), Ok(n)) => m.cmp(&n),
                    // Numeric identifiers sort before alphanumeric ones.
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => i.cmp(j),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
        }
    }
}

impl Ord for Version {
    fn cmp(&self, o: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(o.major, o.minor, o.patch))
            .then_with(|| match (&self.pre, &o.pre) {
                (None, None) => Ordering::Equal,
                // A release is newer than its pre-releases.
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => cmp_pre(a, b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

/// True when `latest` is a newer version than `current`. Unparsable
/// versions are never newer.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (Version::parse(latest), Version::parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Releases

/// A release and the names of its assets, as `.github/workflows/release.yml`
/// builds them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub tag: String,
}

impl Release {
    /// The release of a tag such as `v0.1.8`. The tag must be a plain
    /// version: it goes into URLs and into the tarball member name.
    pub fn for_tag(tag: &str) -> Result<Self> {
        let v = Version::parse(tag).ok_or_else(|| {
            anyhow!("the latest release has the tag {tag:?}, which is not a version")
        })?;
        Ok(Self {
            version: v.to_string(),
            tag: format!("v{v}"),
        })
    }
    pub fn tarball(&self) -> String {
        format!("paddock-{}-x86_64-linux.tar.gz", self.version)
    }
    pub fn checksum(&self) -> String {
        format!("{}.sha256", self.tarball())
    }
    /// The binary inside the tarball.
    pub fn member(&self) -> String {
        format!("paddock-{}-x86_64-linux/paddock", self.version)
    }
    fn asset_url(&self, name: &str) -> String {
        format!("{REPO_URL}/releases/download/{}/{name}", self.tag)
    }
    pub fn tarball_url(&self) -> String {
        self.asset_url(&self.tarball())
    }
    pub fn checksum_url(&self) -> String {
        self.asset_url(&self.checksum())
    }
}

/// Where releases come from. Tests use a fake one: no test opens a network
/// connection.
pub trait Source: Send + Sync {
    fn latest(&self) -> Result<Release>;
    fn download(&self, url: &str) -> Result<Vec<u8>>;
}

// ---------------------------------------------------------------------------
// HTTPS

/// Accept only `https://` URLs on the GitHub hosts, on the default port,
/// without user information.
pub fn check_url(raw: &str) -> Result<Url> {
    let u = Url::parse(raw).with_context(|| format!("{raw} is not a URL"))?;
    if u.scheme() != "https" {
        bail!("refused {raw}: not https");
    }
    if !u.username().is_empty() || u.password().is_some() {
        bail!("refused {raw}: it has user information");
    }
    if u.port().is_some_and(|p| p != 443) {
        bail!("refused {raw}: not port 443");
    }
    let host = u.host_str().unwrap_or("");
    if !ALLOWED_HOSTS.contains(&host) {
        bail!(
            "refused {}: paddock downloads updates only from {}",
            redact(&u),
            ALLOWED_HOSTS.join(", ")
        );
    }
    Ok(u)
}

/// A URL for messages: without the query, which holds signed tokens on the
/// asset hosts.
fn redact(u: &Url) -> String {
    let mut c = u.clone();
    c.set_query(None);
    c.set_fragment(None);
    c.to_string()
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub location: Option<String>,
    pub body: Vec<u8>,
}

/// Decode a chunked body. Errors when the last chunk is missing, so a cut
/// connection is not taken for a whole file.
pub fn dechunk(mut raw: &[u8], max: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let i = raw
            .windows(2)
            .position(|w| w == b"\r\n")
            .context("truncated chunked body")?;
        let line = std::str::from_utf8(&raw[..i])?;
        let size = line.split(';').next().unwrap_or("").trim();
        let n = usize::from_str_radix(size, 16).context("bad chunk size")?;
        raw = &raw[i + 2..];
        if n == 0 {
            return Ok(out);
        }
        if raw.len() < n + 2 {
            bail!("truncated chunked body");
        }
        if out.len() + n > max {
            bail!("the response is larger than {max} bytes");
        }
        out.extend_from_slice(&raw[..n]);
        raw = &raw[n + 2..];
    }
}

/// Parse a whole HTTP/1.1 response. `clean` is false when the connection
/// ended without a TLS close_notify: then the body must have a length.
pub fn parse_response(raw: &[u8], max: usize, clean: bool) -> Result<Response> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("the server closed the connection before the HTTP head ended")?;
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut headers);
    resp.parse(&raw[..end + 4])?;
    let status = resp.code.context("no HTTP status")?;
    let header = |name: &str| {
        resp.headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| String::from_utf8_lossy(h.value).trim().to_string())
    };
    let location = header("location");
    let rest = &raw[end + 4..];
    let body = if header("transfer-encoding").is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
        dechunk(rest, max)?
    } else if let Some(len) = header("content-length") {
        let len: usize = len.parse().context("bad Content-Length")?;
        if len > max {
            bail!("the response is larger than {max} bytes");
        }
        if rest.len() < len {
            bail!("the connection ended after {} of {len} bytes", rest.len());
        }
        rest[..len].to_vec()
    } else {
        if !clean {
            bail!("the connection ended without a length or a TLS close");
        }
        rest.to_vec()
    };
    Ok(Response {
        status,
        location,
        body,
    })
}

/// The GitHub release source: HTTPS through the host proxy settings.
pub struct GitHub {
    pub proxy: HostProxy,
    pub timeout: Duration,
}

impl GitHub {
    pub fn from_env(env: &HostEnv, timeout: Duration) -> Self {
        Self {
            proxy: HostProxy::from_vars(|k| env.var(k).map(str::to_string)),
            timeout,
        }
    }

    fn get_once(&self, url: &Url, accept: &str, max: usize) -> Result<Response> {
        let host = url.host_str().context("no host")?.to_string();
        let tcp = crate::proxy::host_connect(&self.proxy, &host, 443, self.timeout)?;
        let cfg = crate::proxy::tls::system_config()?;
        let name = rustls::pki_types::ServerName::try_from(host.clone())?;
        let conn = rustls::ClientConnection::new(cfg, name)?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        let path = &url[url::Position::BeforePath..url::Position::AfterQuery];
        let ua = format!("paddock/{CURRENT}");
        write!(
            s,
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: {ua}\r\nAccept: {accept}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
        )?;
        s.flush()?;
        let mut raw = Vec::new();
        let mut buf = [0u8; 64 << 10];
        let cap = max + MAX_HEAD;
        let clean = loop {
            match s.read(&mut buf) {
                Ok(0) => break true,
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    if raw.len() > cap {
                        bail!("the response from {host} is larger than {max} bytes");
                    }
                }
                // No close_notify: parse_response then needs a length.
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break false,
                Err(e) => return Err(e).with_context(|| format!("reading from {host}")),
            }
        };
        parse_response(&raw, max, clean)
    }

    /// GET a URL. With `follow`, redirects are followed to allowed hosts only.
    fn get(&self, raw: &str, accept: &str, max: usize, follow: bool) -> Result<Response> {
        let mut url = check_url(raw)?;
        for _ in 0..=MAX_REDIRECTS {
            let r = self
                .get_once(&url, accept, max)
                .with_context(|| format!("GET {}", redact(&url)))?;
            if !follow || !matches!(r.status, 301 | 302 | 303 | 307 | 308) {
                return Ok(r);
            }
            let loc = r.location.context("a redirect without Location")?;
            let next = url.join(&loc)?;
            url = check_url(next.as_str()).context("refused a redirect")?;
        }
        bail!("too many redirects")
    }

    /// The latest release from the `releases/latest` redirect of github.com,
    /// which has no API rate limit.
    fn latest_from_page(&self) -> Result<Release> {
        let r = self.get(
            &format!("{REPO_URL}/releases/latest"),
            "text/html",
            MAX_JSON,
            false,
        )?;
        let loc = r
            .location
            .filter(|_| matches!(r.status, 301 | 302 | 303 | 307 | 308))
            .context("github.com did not name the latest release")?;
        let tag = loc
            .rsplit_once("/releases/tag/")
            .map(|(_, t)| t.to_string())
            .context("github.com did not name the latest release")?;
        Release::for_tag(&tag)
    }
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
}

impl Source for GitHub {
    fn latest(&self) -> Result<Release> {
        let api = format!("https://api.github.com/repos/{REPO}/releases/latest");
        let r = self.get(&api, "application/vnd.github+json", MAX_JSON, true)?;
        match r.status {
            200 => {
                let a: ApiRelease = serde_json::from_slice(&r.body)
                    .context("the GitHub API sent an unexpected answer")?;
                let rel = Release::for_tag(&a.tag_name)?;
                for need in [rel.tarball(), rel.checksum()] {
                    if !a.assets.iter().any(|x| x.name == need) {
                        bail!(
                            "release {} has no {need} yet. The release build may still run; try again later",
                            rel.tag
                        );
                    }
                }
                Ok(rel)
            }
            // The API rate limit for clients without a token.
            403 | 429 => self.latest_from_page(),
            404 => bail!("{REPO} has no release yet"),
            s => bail!("the GitHub API answered with status {s}"),
        }
    }

    fn download(&self, url: &str) -> Result<Vec<u8>> {
        let r = self.get(url, "application/octet-stream", MAX_DOWNLOAD, true)?;
        if r.status != 200 {
            bail!("downloading {url} failed with status {}", r.status);
        }
        Ok(r.body)
    }
}

// ---------------------------------------------------------------------------
// Checks before the replacement

/// Refuse in a sandbox: there the binary is the host's, read-only, and the
/// agent must not replace it.
pub fn refuse_in_sandbox(env: &HostEnv, what: &str) -> Result<()> {
    if env.var("PADDOCK") == Some("1") {
        bail!("{what} runs on the host only. This is a paddock sandbox (PADDOCK=1): ask the user to run it in a terminal on the host");
    }
    Ok(())
}

/// Check the tarball against the `.sha256` file of the release.
pub fn verify_checksum(tarball: &[u8], sums: &[u8], name: &str) -> Result<()> {
    let text = std::str::from_utf8(sums).context("the checksum file is not text")?;
    let expected = text
        .lines()
        .find_map(|l| {
            let mut it = l.split_whitespace();
            let hash = it.next()?;
            let file = it.next()?.trim_start_matches('*');
            (file == name).then(|| hash.to_ascii_lowercase())
        })
        .with_context(|| format!("the checksum file has no line for {name}"))?;
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("the checksum file has no SHA-256 for {name}");
    }
    let got: String = Sha256::digest(tarball)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if got != expected {
        bail!("the checksum of {name} does not match: expected {expected}, got {got}. Nothing was changed");
    }
    Ok(())
}

/// The package that owns `path`, if any: `pacman -Qo`, or any file in /usr.
pub fn package_owner(env: &HostEnv, path: &Path) -> Option<String> {
    if let Some(pacman) = env.which("pacman") {
        let out = Command::new(pacman)
            .args(["-Qo", "--"])
            .arg(path)
            .env("LC_ALL", "C")
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if out.status.success() {
            return Some(String::from_utf8_lossy(&out.stdout).trim().to_string());
        }
    }
    path.starts_with("/usr").then(|| {
        format!(
            "{} is in /usr, which the system package manager owns",
            path.display()
        )
    })
}

/// What to do instead when a package owns the binary.
pub fn package_hint(owner: &str) -> String {
    format!(
        "paddock was installed by a package ({owner}), so paddock update does not overwrite it. Update the package instead: in a clone of {REPO_URL}, run `git pull` and `bin/install`, which builds and installs the new package with makepkg and pacman. To switch to a binary in ~/.local/bin, run `sudo pacman -R omarchy-paddock` first and install the prebuilt binary (see the README)"
    )
}

/// Run `bin --version` and check that it names `version`.
pub fn check_version(bin: &Path, version: &str) -> Result<()> {
    let mut child = Command::new(bin)
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot run the downloaded {}", bin.display()))?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the downloaded paddock did not answer --version");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut out = String::new();
    if let Some(s) = child.stdout.take() {
        s.take(4096).read_to_string(&mut out)?;
    }
    let want = format!("paddock {version}");
    if out.trim() != want {
        bail!(
            "the downloaded binary prints {:?} for --version, not {want:?}. Nothing was changed",
            out.trim()
        );
    }
    Ok(())
}

/// Extract one member of a .tar.gz to memory with the system `tar`. Nothing
/// of the archive is written to the file system.
pub fn extract_member(tar: &Path, tarball: &Path, member: &str) -> Result<Vec<u8>> {
    let out = Command::new(tar)
        .args(["-xzOf"])
        .arg(tarball)
        .arg("--")
        .arg(member)
        .stdin(Stdio::null())
        .output()
        .context("cannot run tar")?;
    if !out.status.success() || out.stdout.is_empty() {
        bail!(
            "the tarball has no {member}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// Replace `target` atomically: a new file next to it with mode 0755,
/// synced to disk, then renamed over it. A running paddock keeps the old
/// file open; the next start uses the new one.
pub fn replace_binary(target: &Path, bytes: &[u8]) -> Result<()> {
    let dir = target
        .parent()
        .context("the paddock binary has no folder")?;
    let tmp = dir.join(format!(".paddock-update-{}", std::process::id()));
    let write = || -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&tmp)
            .with_context(|| format!("cannot write to {}", dir.display()))?;
        f.write_all(bytes)?;
        // The umask may have taken bits away.
        f.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, target)
            .with_context(|| format!("cannot replace {}", target.display()))?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    };
    let r = write();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// A private 0700 folder under paddock's cache, removed on drop.
pub struct PrivTemp(PathBuf);

impl PrivTemp {
    pub fn new(env: &HostEnv) -> Result<Self> {
        let p = env.paddock_cache().join("update").join(format!(
            "{}-{}",
            std::process::id(),
            crate::state::now()
        ));
        if p.exists() {
            std::fs::remove_dir_all(&p)?;
        }
        crate::privfs::dir(&p)?;
        Ok(Self(p))
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PrivTemp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Download, check and install `rel` over `target`. `owner` is the package
/// that owns `target`, if any: then nothing is downloaded.
pub fn install(
    env: &HostEnv,
    source: &dyn Source,
    rel: &Release,
    target: &Path,
    owner: Option<&str>,
    tar: &Path,
) -> Result<()> {
    refuse_in_sandbox(env, "paddock update")?;
    if let Some(o) = owner {
        bail!("{}", package_hint(o));
    }
    let tmp = PrivTemp::new(env)?;
    let tarball = source.download(&rel.tarball_url())?;
    let sums = source.download(&rel.checksum_url())?;
    verify_checksum(&tarball, &sums, &rel.tarball())?;
    let tar_file = tmp.path().join(rel.tarball());
    crate::privfs::write(&tar_file, &tarball)?;
    let bin = extract_member(tar, &tar_file, &rel.member())?;
    let probe = tmp.path().join("paddock");
    crate::privfs::write(&probe, &bin)?;
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o700))?;
    check_version(&probe, &rel.version)?;
    replace_binary(target, &bin)
}

/// The binary that `paddock update` replaces: this program.
pub fn current_binary() -> Result<PathBuf> {
    std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .context("cannot find the paddock executable")
}

// ---------------------------------------------------------------------------
// The bar plugin

/// Whether the Omarchy bar plugin is installed. None without Omarchy.
pub fn plugin_installed(env: &HostEnv) -> Option<bool> {
    let omarchy = env.which("omarchy")?;
    let out = Command::new(omarchy)
        .args(["plugin", "list", "--json"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(
        v.as_array()?
            .iter()
            .any(|p| p.get("id").and_then(|i| i.as_str()) == Some(PLUGIN_ID)),
    )
}

/// Update the bar plugin with `omarchy plugin update`.
pub fn update_plugin(env: &HostEnv) -> Result<()> {
    let omarchy = env.which("omarchy").context("omarchy is not on PATH")?;
    let st = Command::new(omarchy)
        .args(["plugin", "update", PLUGIN_ID, "--yes"])
        .status()?;
    if !st.success() {
        bail!("omarchy plugin update {PLUGIN_ID} failed with {st}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The notice in `paddock doctor`

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Cache {
    checked: u64,
    latest: Option<String>,
}

fn cache_file(env: &HostEnv) -> PathBuf {
    env.paddock_cache().join("update-check.json")
}

fn read_cache(path: &Path) -> Cache {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

const DAY: u64 = 24 * 60 * 60;

/// The latest release version, from a cache that is at most a day old, or
/// from `fetch`. The fetch runs in a thread and gets `wait` to answer; after
/// that the last known version is used, and the thread writes the cache if
/// it still finishes before paddock exits. Every attempt counts as the
/// check of the day, so an offline machine does not wait at every run.
pub fn latest_cached(
    env: &HostEnv,
    now: u64,
    wait: Duration,
    fetch: impl FnOnce() -> Result<String> + Send + 'static,
) -> Option<String> {
    let file = cache_file(env);
    let old = read_cache(&file);
    if now.saturating_sub(old.checked) < DAY && old.checked <= now {
        return old.latest;
    }
    let attempt = Cache {
        checked: now,
        latest: old.latest.clone(),
    };
    let _ = crate::privfs::write_atomic(&file, serde_json::to_string(&attempt).unwrap_or_default());
    let (tx, rx) = std::sync::mpsc::channel();
    let f = file.clone();
    std::thread::spawn(move || {
        let r = fetch();
        if let Ok(v) = &r {
            let c = Cache {
                checked: now,
                latest: Some(v.clone()),
            };
            let _ = crate::privfs::write_atomic(&f, serde_json::to_string(&c).unwrap_or_default());
        }
        let _ = tx.send(r);
    });
    match rx.recv_timeout(wait) {
        Ok(Ok(v)) => Some(v),
        _ => old.latest,
    }
}

/// The lines of the "Updates" section of `paddock doctor`.
pub fn doctor_lines(env: &HostEnv) -> Vec<String> {
    let mut out = Vec::new();
    if env.var("PADDOCK") == Some("1") {
        out.push("  skipped  not checked inside a sandbox".to_string());
    } else if env.var("PADDOCK_NO_UPDATE_CHECK") == Some("1") {
        out.push(
            "  off      PADDOCK_NO_UPDATE_CHECK=1 turns off the check for a newer release"
                .to_string(),
        );
    } else {
        let gh = GitHub::from_env(env, Duration::from_secs(5));
        let latest = latest_cached(
            env,
            crate::state::now(),
            Duration::from_secs(2),
            move || gh.latest().map(|r| r.version),
        );
        out.push(release_line(latest.as_deref(), CURRENT));
    }
    out.extend(app_restart_hints(env));
    out
}

pub fn release_line(latest: Option<&str>, current: &str) -> String {
    match latest {
        Some(l) if is_newer(l, current) => format!(
            "  new      paddock {l} is out, this is {current}. See the steps with: paddock update --check"
        ),
        Some(_) => format!("  ok       paddock {current} is the latest release"),
        None => format!(
            "  unknown  paddock {current}; could not check for a newer release. PADDOCK_NO_UPDATE_CHECK=1 turns the check off"
        ),
    }
}

// ---------------------------------------------------------------------------
// Apps that changed on the host while their sandbox runs

/// The time a file last changed on disk. ctime, not mtime: package managers
/// keep the mtime of the package, but they cannot set the ctime.
fn changed_at(p: &Path) -> Option<u64> {
    let mut t = std::fs::symlink_metadata(p).ok()?.ctime();
    if let Ok(m) = std::fs::metadata(p) {
        t = t.max(m.ctime());
    }
    u64::try_from(t).ok()
}

/// True when any of `paths` changed after `started`.
pub fn changed_after(paths: &[PathBuf], started: u64) -> bool {
    paths
        .iter()
        .filter_map(|p| changed_at(p))
        .any(|t| t > started)
}

/// The files that change when an app on the host is updated.
fn app_files(env: &HostEnv, factory: &crate::profile::HarnessFactory, name: &str) -> Vec<PathBuf> {
    let Some(p) = factory.profile(name) else {
        return vec![];
    };
    let Some(app) = &p.app else { return vec![] };
    if let Some(id) = &app.flatpak {
        // flatpak points `current/active` at the new deployment.
        return vec![
            env.data_dir
                .join("flatpak/app")
                .join(id)
                .join("current/active"),
            PathBuf::from("/var/lib/flatpak/app")
                .join(id)
                .join("current/active"),
        ];
    }
    match factory.create(name) {
        Ok(h) => {
            let mut v = vec![h.exe.clone()];
            if let Ok(c) = std::fs::canonicalize(&h.exe) {
                v.push(c);
            }
            v
        }
        Err(_) => vec![],
    }
}

/// A line for each running app sandbox whose app changed on the host since
/// the sandbox started.
pub fn app_restart_hints(env: &HostEnv) -> Vec<String> {
    let running: Vec<_> = crate::state::Running::list(env)
        .into_iter()
        .filter(|r| r.gui == "app")
        .collect();
    if running.is_empty() {
        return vec![];
    }
    let Ok(factory) = crate::profile::HarnessFactory::new(env) else {
        return vec![];
    };
    running
        .iter()
        .filter(|r| changed_after(&app_files(env, &factory, &r.harness), r.started))
        .map(|r| {
            format!(
                "  restart  {} ({}) was updated on the host after its sandbox started. Close it and start it again to use the new version",
                r.harness, r.id
            )
        })
        .collect()
}

#[cfg(test)]
mod tests;
