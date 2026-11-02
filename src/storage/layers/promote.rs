//! Promotion: after a sandbox ends, copy the verified new packages of a
//! project's upper layers into the shared lower layers.
//!
//! A file reaches the shared layer only when its content matches a source
//! that no project can change:
//!
//! - cargo: `cache/<index dir>/<name>-<version>.crate` of the crates.io
//!   index, against the `cksum` of the sparse index entry fetched on the
//!   host. Never the project's `Cargo.lock` or its copy of the index.
//! - npm: `_cacache/content-v2/<algo>/<hex path>`, against the hash in its
//!   own path (sha512 and sha256 only).
//! - go: `cache/download/<module>/@v/<version>.{zip,mod}`, against the `h1:`
//!   hashes of `sum.golang.org`, fetched on the host.
//!
//! Anything else stays in the project. A copy never replaces a file that is
//! already in the lower layer. Each result goes to the audit log, without
//! content.

use super::{lower, upper};
use crate::env::HostEnv;
use crate::safefs::{Dir, Kind, Root};
use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde::Serialize;
use sha2::{Digest, Sha256, Sha512};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The time limit of one promotion run.
pub const LIMIT: Duration = Duration::from_secs(120);
/// The timeout of one network request.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Larger package files are not checked.
const MAX_FILE: u64 = 512 << 20;
const MAX_INDEX: usize = 8 << 20;

/// Where the trusted hashes come from. A test seam: tests point both at a
/// local server.
#[derive(Debug, Clone)]
pub struct Sources {
    pub crates_index: String,
    pub sumdb: String,
}

impl Default for Sources {
    fn default() -> Self {
        Self {
            crates_index: "https://index.crates.io".into(),
            sumdb: "https://sum.golang.org".into(),
        }
    }
}

/// A GET request. `Ok(None)` when the server says 404 or 410.
pub trait Fetch {
    fn get(&self, url: &str) -> Result<Option<Vec<u8>>>;
}

/// HTTPS through the host's proxy settings. Plain HTTP only to 127.0.0.1,
/// for the local servers of tests.
pub struct Http {
    pub proxy: crate::proxy::HostProxy,
    pub timeout: Duration,
}

impl Http {
    pub fn from_env(env: &HostEnv) -> Self {
        Self {
            proxy: crate::proxy::HostProxy::from_vars(|k| env.var(k).map(str::to_string)),
            timeout: TIMEOUT,
        }
    }
}

impl Fetch for Http {
    fn get(&self, raw: &str) -> Result<Option<Vec<u8>>> {
        let url = url::Url::parse(raw)?;
        let host = url.host_str().context("no host")?.to_string();
        let path = &url[url::Position::BeforePath..url::Position::AfterQuery];
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: paddock\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
        );
        let mut raw_resp = Vec::new();
        let clean = match url.scheme() {
            "https" => {
                let tcp = crate::proxy::host_connect(&self.proxy, &host, 443, self.timeout)?;
                let cfg = crate::proxy::tls::system_config()?;
                let name = rustls::pki_types::ServerName::try_from(host.clone())?;
                let conn = rustls::ClientConnection::new(cfg, name)?;
                let mut s = rustls::StreamOwned::new(conn, tcp);
                s.write_all(req.as_bytes())?;
                read_all(&mut s, &mut raw_resp)?
            }
            "http" if host == "127.0.0.1" => {
                let port = url.port().context("no port")?;
                let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
                s.set_read_timeout(Some(self.timeout))?;
                s.write_all(req.as_bytes())?;
                read_all(&mut s, &mut raw_resp)?
            }
            other => bail!("refused: {other}://{host}"),
        };
        let r = crate::update::parse_response(&raw_resp, MAX_INDEX, clean)?;
        match r.status {
            200 => Ok(Some(r.body)),
            404 | 410 => Ok(None),
            s => bail!("{host} answered {s}"),
        }
    }
}

fn read_all(s: &mut impl Read, out: &mut Vec<u8>) -> Result<bool> {
    let mut buf = [0u8; 64 << 10];
    loop {
        match s.read(&mut buf) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > MAX_INDEX + (64 << 10) {
                    bail!("the response is too large");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(e) => return Err(e.into()),
        }
    }
}

/// The result for one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub cache: String,
    pub package: String,
    pub version: String,
    pub result: String,
}

/// One candidate file in an upper layer.
struct Candidate {
    cache: &'static str,
    /// The path below the upper and the lower folder.
    rel: PathBuf,
    package: String,
    version: String,
    check: Check,
}

enum Check {
    /// Crate from the crates.io index.
    Crate,
    /// npm content: the algorithm and the hex digest from the path.
    Content(String, String),
    GoZip {
        escaped: String,
    },
    GoMod {
        escaped: String,
    },
}

/// The record of checked files, so a package version is fetched once.
fn record_path(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("layers-checked.json")
}

type Record = BTreeMap<String, String>;

/// The record keeps at most this many results. Over it, the refusals go
/// first, then everything: the record only saves requests.
const MAX_RECORD: usize = 20_000;

fn load_record(env: &HostEnv) -> Record {
    std::fs::read(record_path(env))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_record(env: &HostEnv, r: &mut Record) {
    if r.len() > MAX_RECORD {
        r.retain(|_, v| v == "ok");
    }
    if r.len() > MAX_RECORD {
        r.clear();
    }
    let p = record_path(env);
    if let Some(d) = p.parent() {
        let _ = crate::privfs::dir(d);
    }
    if let Ok(b) = serde_json::to_vec(r) {
        let _ = crate::privfs::write_atomic(&p, b);
    }
}

/// Promote what can be verified in the upper layers of `project`.
pub fn run(
    env: &HostEnv,
    project: &Path,
    sources: &Sources,
    fetch: &dyn Fetch,
    deadline: Instant,
) -> Vec<Outcome> {
    let mut out = Vec::new();
    let mut record = load_record(env);
    let mut index_cache: BTreeMap<String, Option<String>> = BTreeMap::new();
    for cache in ["cargo-registry", "npm", "go-mod"] {
        let up = upper(env, project, cache);
        let Ok(root) = Root::open(&up) else { continue };
        let low = lower(env, cache);
        if crate::privfs::dir(&low).is_err() {
            continue;
        }
        let Ok(low_root) = Root::open(&low) else {
            continue;
        };
        for c in candidates(cache, &root) {
            if Instant::now() > deadline {
                save_record(env, &mut record);
                return out;
            }
            let result = promote_one(
                &c,
                &root,
                &low_root,
                sources,
                fetch,
                &mut record,
                &mut index_cache,
            );
            let result = match result {
                Ok(None) => continue,
                Ok(Some(r)) => r,
                Err(e) => format!("unchecked: {e:#}"),
            };
            let o = Outcome {
                cache: cache.into(),
                package: c.package.clone(),
                version: c.version.clone(),
                result,
            };
            audit(env, project, &o);
            out.push(o);
        }
    }
    save_record(env, &mut record);
    out
}

fn audit(env: &HostEnv, project: &Path, o: &Outcome) {
    crate::broker::audit_line(
        env,
        &serde_json::json!({
            "time": crate::state::now(),
            "project": project,
            "capability": "cache-promotion",
            "cache": o.cache,
            "package": o.package,
            "version": o.version,
            "decision": o.result,
        }),
    );
}

/// `Ok(None)` when there is nothing to do: the same file is in the lower
/// layer already. `Ok(Some(result))` otherwise.
fn promote_one(
    c: &Candidate,
    up: &Root,
    low: &Root,
    sources: &Sources,
    fetch: &dyn Fetch,
    record: &mut Record,
    index_cache: &mut BTreeMap<String, Option<String>>,
) -> Result<Option<String>> {
    let Some(data) = up.read(&c.rel, MAX_FILE)? else {
        return Ok(None);
    };
    match low.read(&c.rel, MAX_FILE) {
        Ok(Some(have)) if have == data => return Ok(None),
        Ok(Some(_)) => return Ok(Some("refused: another file is in the shared layer".into())),
        Ok(None) => {}
        Err(_) => return Ok(Some("refused: the shared layer has no file there".into())),
    }
    let sha = crate::manifest::hex(&Sha256::digest(&data));
    let key = format!("{}|{}|{}|{sha}", c.cache, c.package, c.version);
    let verdict = match record.get(&key) {
        Some(v) => v.clone(),
        None => {
            let v = verify(c, &data, &sha, sources, fetch, index_cache)?;
            record.insert(key, v.clone());
            v
        }
    };
    if verdict != "ok" {
        return Ok(Some(format!("refused: {verdict}")));
    }
    place(low, &c.rel, &data)?;
    Ok(Some("promoted".into()))
}

/// "ok", or why the content does not match. Network errors are errors, so
/// they are not recorded and the next run tries again.
fn verify(
    c: &Candidate,
    data: &[u8],
    sha: &str,
    sources: &Sources,
    fetch: &dyn Fetch,
    index_cache: &mut BTreeMap<String, Option<String>>,
) -> Result<String> {
    Ok(match &c.check {
        Check::Crate => {
            let url = format!(
                "{}/{}",
                sources.crates_index.trim_end_matches('/'),
                index_path(&c.package).context("not a crate name")?
            );
            let entry = match index_cache.get(&url) {
                Some(e) => e.clone(),
                None => {
                    let e = fetch
                        .get(&url)?
                        .map(|b| String::from_utf8_lossy(&b).into_owned());
                    index_cache.insert(url, e.clone());
                    e
                }
            };
            match entry.and_then(|e| index_cksum(&e, &c.version)) {
                Some(ck) if ck.eq_ignore_ascii_case(sha) => "ok".into(),
                Some(_) => "the cksum of the crates.io index differs".into(),
                None => "the crates.io index has no such version".into(),
            }
        }
        Check::Content(algo, hex) => {
            let got = match algo.as_str() {
                "sha512" => crate::manifest::hex(&Sha512::digest(data)),
                "sha256" => sha.to_string(),
                _ => return Ok("a weak or unknown hash".into()),
            };
            if got == *hex {
                "ok".into()
            } else {
                "the content does not match its hash".into()
            }
        }
        Check::GoZip { escaped } | Check::GoMod { escaped } => {
            let esc_version = escape(&c.version);
            let url = format!(
                "{}/lookup/{escaped}@{esc_version}",
                sources.sumdb.trim_end_matches('/')
            );
            let Some(body) = fetch.get(&url)? else {
                return Ok("sum.golang.org does not know this version".into());
            };
            let body = String::from_utf8_lossy(&body);
            let gomod = matches!(c.check, Check::GoMod { .. });
            let Some(want) = sumdb_hash(&body, &c.package, &c.version, gomod) else {
                return Ok("sum.golang.org gave no hash".into());
            };
            let got = if gomod {
                hash1(&[("go.mod".to_string(), data.to_vec())])
            } else {
                match super::zip::files(data) {
                    Ok(f) => hash1(&f),
                    Err(e) => return Ok(format!("not a module zip: {e}")),
                }
            };
            match got {
                Some(g) if g == want => "ok".into(),
                _ => "the h1 hash of sum.golang.org differs".into(),
            }
        }
    })
}

/// Write `data` at `rel` in the lower layer: a new temporary file, then a
/// rename that never replaces an existing entry.
fn place(low: &Root, rel: &Path, data: &[u8]) -> Result<()> {
    let parent = rel.parent().unwrap_or(Path::new(""));
    let name = rel.file_name().context("no file name")?;
    let dir: Dir = low.dir(parent, true)?;
    let tmp = OsString::from(format!(".paddock-promote-{}", std::process::id()));
    let _ = dir.remove(&tmp);
    let mut f = dir.create_new(&tmp, 0o644)?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    let r = dir.move_in(&dir.path().join(&tmp), name);
    let _ = dir.remove(&tmp);
    match r {
        // Another promotion placed the same file meanwhile.
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
            if dir.read(name, MAX_FILE)?.as_deref() == Some(data) {
                return Ok(());
            }
            bail!("another file is in the shared layer")
        }
        r => r.with_context(|| format!("cannot place {}", rel.display())),
    }
}

/// The files of `cache` in an upper layer that promotion can check.
fn candidates(cache: &'static str, root: &Root) -> Vec<Candidate> {
    let mut out = Vec::new();
    match cache {
        "cargo-registry" => {
            for (idx, _) in entries(root, Path::new("cache"), Kind::Dir) {
                // Only the crates.io index, by the name cargo gives its folder.
                if !idx.to_string_lossy().starts_with("index.crates.io-") {
                    continue;
                }
                let d = Path::new("cache").join(&idx);
                for (f, _) in entries(root, &d, Kind::File) {
                    let f = f.to_string_lossy().into_owned();
                    let Some(stem) = f.strip_suffix(".crate") else {
                        continue;
                    };
                    for (package, version) in crate_splits(stem) {
                        out.push(Candidate {
                            cache,
                            rel: d.join(&f),
                            package,
                            version,
                            check: Check::Crate,
                        });
                    }
                }
            }
        }
        "npm" => {
            let base = Path::new("_cacache/content-v2");
            for (algo, _) in entries(root, base, Kind::Dir) {
                let mut stack = vec![(base.join(&algo), String::new())];
                while let Some((dir, hex)) = stack.pop() {
                    if hex.len() > 200 {
                        continue;
                    }
                    for (n, k) in entries_any(root, &dir) {
                        let s = n.to_string_lossy().into_owned();
                        if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                            continue;
                        }
                        let full = format!("{hex}{s}");
                        match k {
                            Kind::Dir => stack.push((dir.join(&n), full)),
                            Kind::File => out.push(Candidate {
                                cache,
                                rel: dir.join(&n),
                                package: format!("{}:{}", algo.to_string_lossy(), full),
                                version: String::new(),
                                check: Check::Content(algo.to_string_lossy().into_owned(), full),
                            }),
                            _ => {}
                        }
                    }
                }
            }
        }
        "go-mod" => {
            let base = Path::new("cache/download");
            let mut stack = vec![base.to_path_buf()];
            while let Some(dir) = stack.pop() {
                if dir.components().count() > 40 {
                    continue;
                }
                for (n, k) in entries_any(root, &dir) {
                    if k != Kind::Dir {
                        continue;
                    }
                    if n != OsStr::new("@v") {
                        stack.push(dir.join(&n));
                        continue;
                    }
                    let Ok(escaped) = dir.strip_prefix(base) else {
                        continue;
                    };
                    let escaped = escaped.to_string_lossy().into_owned();
                    let Some(module) = unescape(&escaped) else {
                        continue;
                    };
                    let v = dir.join("@v");
                    for (f, _) in entries(root, &v, Kind::File) {
                        let f = f.to_string_lossy().into_owned();
                        let (ver, check) = if let Some(s) = f.strip_suffix(".zip") {
                            (
                                s,
                                Check::GoZip {
                                    escaped: escaped.clone(),
                                },
                            )
                        } else if let Some(s) = f.strip_suffix(".mod") {
                            (
                                s,
                                Check::GoMod {
                                    escaped: escaped.clone(),
                                },
                            )
                        } else {
                            continue;
                        };
                        let Some(version) = unescape(ver) else {
                            continue;
                        };
                        out.push(Candidate {
                            cache,
                            rel: v.join(&f),
                            package: module.clone(),
                            version,
                            check,
                        });
                    }
                }
            }
        }
        _ => {}
    }
    out
}

fn entries_any(root: &Root, rel: &Path) -> Vec<(OsString, Kind)> {
    root.dir(rel, false)
        .and_then(|d| d.entries())
        .unwrap_or_default()
}

fn entries(root: &Root, rel: &Path, want: Kind) -> Vec<(OsString, Kind)> {
    let mut v: Vec<_> = entries_any(root, rel)
        .into_iter()
        .filter(|(_, k)| *k == want)
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// The ways to split `<name>-<version>`: at each `-` before a digit. The
/// index then decides which split names a real version.
fn crate_splits(stem: &str) -> Vec<(String, String)> {
    stem.match_indices('-')
        .filter(|(i, _)| stem[i + 1..].starts_with(|c: char| c.is_ascii_digit()))
        .map(|(i, _)| (stem[..i].to_string(), stem[i + 1..].to_string()))
        .filter(|(n, _)| index_path(n).is_some())
        .collect()
}

/// The path of a crate in the sparse index, by cargo's prefix rules, in
/// lower case.
pub fn index_path(name: &str) -> Option<String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    let n = name.to_ascii_lowercase();
    Some(match n.len() {
        1 => format!("1/{n}"),
        2 => format!("2/{n}"),
        3 => format!("3/{}/{n}", &n[..1]),
        _ => format!("{}/{}/{n}", &n[..2], &n[2..4]),
    })
}

/// The `cksum` of `version` in a sparse index entry: one JSON object per line.
pub fn index_cksum(entry: &str, version: &str) -> Option<String> {
    entry
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v.get("vers").and_then(|x| x.as_str()) == Some(version))
        .and_then(|v| v.get("cksum")?.as_str().map(str::to_string))
}

/// Go's case escaping of module paths and versions: `!x` is `X`.
pub fn unescape(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut bang = false;
    for c in s.chars() {
        if bang {
            if !c.is_ascii_lowercase() {
                return None;
            }
            out.push(c.to_ascii_uppercase());
            bang = false;
        } else if c == '!' {
            bang = true;
        } else if c.is_ascii_uppercase() {
            return None;
        } else {
            out.push(c);
        }
    }
    (!bang).then_some(out)
}

pub fn escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('!');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The `h1:` hash for `module version` (or its go.mod) in a lookup answer.
pub fn sumdb_hash(body: &str, module: &str, version: &str, gomod: bool) -> Option<String> {
    let want = if gomod {
        format!("{version}/go.mod")
    } else {
        version.to_string()
    };
    body.lines().find_map(|l| {
        let mut f = l.split(' ');
        match (f.next(), f.next(), f.next(), f.next()) {
            (Some(m), Some(v), Some(h), None) if m == module && v == want => {
                h.starts_with("h1:").then(|| h.to_string())
            }
            _ => None,
        }
    })
}

/// Go's dirhash `Hash1`: SHA-256 over the sorted lines
/// `"<hex sha256 of file>  <name>\n"`, in base64 with the prefix `h1:`.
/// None for a name with a newline, which Go refuses too.
pub fn hash1(files: &[(String, Vec<u8>)]) -> Option<String> {
    let mut lines: Vec<(&str, String)> = Vec::with_capacity(files.len());
    for (name, data) in files {
        if name.contains('\n') {
            return None;
        }
        lines.push((name, crate::manifest::hex(&Sha256::digest(data))));
    }
    lines.sort();
    let mut h = Sha256::new();
    for (name, sum) in lines {
        h.update(format!("{sum}  {name}\n"));
    }
    Some(format!(
        "h1:{}",
        base64::engine::general_purpose::STANDARD.encode(h.finalize())
    ))
}

/// At the end of a sandbox: start `paddock promote-caches` in the
/// background, at the lowest priority, and return at once. Only from the
/// real paddock binary, and only after a bwrap sandbox with cache overlays:
/// the other backends have no shared layer.
pub fn spawn_after_run(env: &HostEnv, spec: &crate::spec::SandboxSpec, backend: &str) {
    if !wanted(env, spec, backend) {
        return;
    }
    let Ok(me) = std::env::current_exe() else {
        return;
    };
    if me.file_name().and_then(|n| n.to_str()) != Some("paddock") {
        return;
    }
    let mut cmd = std::process::Command::new(me);
    cmd.arg("promote-caches")
        .arg(&spec.project)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe. The run outlives the sandbox.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    if let Ok(mut child) = cmd.spawn() {
        std::thread::spawn(move || child.wait());
    }
}

/// Promotion runs after this sandbox: bwrap, layered caches, an overlay of
/// a cache that promotion checks.
fn wanted(env: &HostEnv, spec: &crate::spec::SandboxSpec, backend: &str) -> bool {
    if backend != "bwrap" || env.var("PADDOCK_NO_PROMOTE").is_some() || !super::enabled(env) {
        return false;
    }
    let ups: Vec<PathBuf> = ["cargo-registry", "npm", "go-mod"]
        .iter()
        .map(|c| upper(env, &spec.project, c))
        .collect();
    spec.mounts.iter().any(|m| {
        m.kind == crate::spec::MountKind::Overlay && m.src.as_ref().is_some_and(|s| ups.contains(s))
    })
}

/// The hidden command `paddock promote-caches`. A source other than the
/// default must be a local test server.
pub fn command(
    env: &HostEnv,
    project: &Path,
    crates_index: Option<String>,
    sumdb: Option<String>,
) -> Result<Vec<Outcome>> {
    let mut sources = Sources::default();
    for (v, slot) in [
        (crates_index, &mut sources.crates_index),
        (sumdb, &mut sources.sumdb),
    ] {
        if let Some(v) = v {
            if !v.starts_with("http://127.0.0.1:") {
                bail!("a test source must be http://127.0.0.1:PORT");
            }
            *slot = v;
        }
    }
    if !super::project_dir(env, project).is_dir() {
        return Ok(vec![]);
    }
    let _lock = lock(env)?;
    Ok(run(
        env,
        project,
        &sources,
        &Http::from_env(env),
        Instant::now() + LIMIT,
    ))
}

/// One promotion at a time on this host. Waits for a run that holds it.
fn lock(env: &HostEnv) -> Result<std::fs::File> {
    let dir = env.paddock_state();
    crate::privfs::dir(&dir)?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("layers-promote.lock"))?;
    // SAFETY: flock on an open file descriptor.
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&f), libc::LOCK_EX) } != 0 {
        bail!("cannot lock: {}", std::io::Error::last_os_error());
    }
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::super::{mount_with, test_env};
    use super::*;
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A local index server: path to body, 404 for anything else. Counts
    /// the requests.
    struct Server {
        base: String,
        hits: Arc<Mutex<Vec<String>>>,
    }

    fn serve(routes: HashMap<String, String>) -> Server {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        let hits = Arc::new(Mutex::new(Vec::new()));
        let h = hits.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { continue };
                let mut buf = Vec::new();
                let mut b = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut b) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&b[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let path = head.split(' ').nth(1).unwrap_or("").to_string();
                h.lock().unwrap().push(path.clone());
                let (code, body) = match routes.get(&path) {
                    Some(b) => ("200 OK", b.clone()),
                    None => ("404 Not Found", String::new()),
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {code}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Server { base, hits }
    }

    fn http() -> Http {
        Http {
            proxy: crate::proxy::HostProxy::from_vars(|_| None),
            timeout: Duration::from_secs(5),
        }
    }

    /// A project with layered caches in a temporary folder.
    fn project(t: &Path, cache: &str) -> (HostEnv, PathBuf) {
        let env = test_env(t);
        let proj = t.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        mount_with(&env, &proj, cache, "/h/c".into(), "t", true).unwrap();
        (env, proj)
    }

    fn put(dir: &Path, rel: &str, data: &[u8]) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    fn sha(d: &[u8]) -> String {
        crate::manifest::hex(&Sha256::digest(d))
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    #[test]
    fn index_paths_follow_the_sparse_index_prefix_rules() {
        assert_eq!(index_path("a").unwrap(), "1/a");
        assert_eq!(index_path("ab").unwrap(), "2/ab");
        assert_eq!(index_path("abc").unwrap(), "3/a/abc");
        assert_eq!(index_path("serde").unwrap(), "se/rd/serde");
        assert_eq!(index_path("Serde_JSON").unwrap(), "se/rd/serde_json");
        assert_eq!(index_path("cc-1").unwrap(), "cc/-1/cc-1");
        for bad in ["", "a/b", "..", "a.b", "a%2f", &"x".repeat(65)] {
            assert_eq!(index_path(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_crate_file_name_splits_before_each_version() {
        assert_eq!(
            crate_splits("serde-1.0.200"),
            vec![("serde".into(), "1.0.200".into())]
        );
        assert_eq!(
            crate_splits("foo-bar-1.0.0-alpha.1"),
            vec![("foo-bar".into(), "1.0.0-alpha.1".into())]
        );
        assert_eq!(
            crate_splits("a-1-2.0.0"),
            vec![
                ("a".into(), "1-2.0.0".into()),
                ("a-1".into(), "2.0.0".into())
            ]
        );
        assert!(crate_splits("noversion").is_empty());
    }

    #[test]
    fn the_index_entry_gives_the_cksum_of_one_version() {
        let e = "{\"name\":\"x\",\"vers\":\"1.0.0\",\"cksum\":\"aa\"}\nnot json\n{\"name\":\"x\",\"vers\":\"1.1.0\",\"cksum\":\"bb\"}\n";
        assert_eq!(index_cksum(e, "1.1.0").as_deref(), Some("bb"));
        assert_eq!(index_cksum(e, "1.0.0").as_deref(), Some("aa"));
        assert_eq!(index_cksum(e, "2.0.0"), None);
    }

    #[test]
    fn crates_match_the_cksum_of_the_index_and_a_tampered_one_stays() {
        let t = tempfile::tempdir().unwrap();
        let (env, proj) = project(t.path(), "cargo-registry");
        let up = upper(&env, &proj, "cargo-registry");
        let dir = "cache/index.crates.io-1949cf8c6b5b557f";
        let (good, bad) = (b"good crate".as_slice(), b"planted code".as_slice());
        put(&up, &format!("{dir}/good-1.0.0.crate"), good);
        put(&up, &format!("{dir}/evil-2.0.0.crate"), bad);
        // Another registry is never checked.
        put(&up, "cache/my.registry-0123/good-1.0.0.crate", good);
        let srv = serve(HashMap::from([
            (
                "/go/od/good".to_string(),
                format!(
                    "{{\"name\":\"good\",\"vers\":\"1.0.0\",\"cksum\":\"{}\"}}\n",
                    sha(good)
                ),
            ),
            (
                "/ev/il/evil".to_string(),
                format!(
                    "{{\"name\":\"evil\",\"vers\":\"2.0.0\",\"cksum\":\"{}\"}}\n",
                    sha(b"the real crate")
                ),
            ),
        ]));
        let src = Sources {
            crates_index: srv.base.clone(),
            sumdb: "http://127.0.0.1:9".into(),
        };
        let mut r = run(&env, &proj, &src, &http(), later());
        r.sort_by(|a, b| a.package.cmp(&b.package));
        let res: Vec<_> = r
            .iter()
            .map(|o| (o.package.as_str(), o.version.as_str(), o.result.as_str()))
            .collect();
        assert_eq!(
            res,
            vec![
                (
                    "evil",
                    "2.0.0",
                    "refused: the cksum of the crates.io index differs"
                ),
                ("good", "1.0.0", "promoted"),
            ]
        );
        let low = lower(&env, "cargo-registry");
        assert_eq!(
            std::fs::read(low.join(dir).join("good-1.0.0.crate")).unwrap(),
            good
        );
        assert!(!low.join(dir).join("evil-2.0.0.crate").exists());
        assert!(!low.join("cache/my.registry-0123").exists());

        // The audit log names each result, never the content.
        let log = std::fs::read_to_string(crate::broker::audit_file(&env)).unwrap();
        assert_eq!(log.matches("\"cache-promotion\"").count(), 2, "{log}");
        assert!(log.contains("\"package\":\"evil\""), "{log}");
        assert!(
            !log.contains("planted") && !log.contains("good crate"),
            "{log}"
        );

        // A second run asks the index nothing: the record keeps the refusal,
        // and the good file is in the shared layer already.
        let before = srv.hits.lock().unwrap().len();
        let r = run(&env, &proj, &src, &http(), later());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].package, "evil");
        assert!(r[0].result.starts_with("refused:"));
        assert_eq!(srv.hits.lock().unwrap().len(), before);
        let rec = load_record(&env);
        assert!(
            rec.keys().all(|k| k.starts_with("cargo-registry|")),
            "{rec:?}"
        );
    }

    #[test]
    fn an_unreachable_index_promotes_nothing_and_records_nothing() {
        let t = tempfile::tempdir().unwrap();
        let (env, proj) = project(t.path(), "cargo-registry");
        let up = upper(&env, &proj, "cargo-registry");
        put(
            &up,
            "cache/index.crates.io-1949cf8c6b5b557f/good-1.0.0.crate",
            b"x",
        );
        // A port that nothing listens on.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let src = Sources {
            crates_index: format!("http://127.0.0.1:{port}"),
            sumdb: String::new(),
        };
        let r = run(&env, &proj, &src, &http(), later());
        assert_eq!(r.len(), 1);
        assert!(r[0].result.starts_with("unchecked:"), "{r:?}");
        assert!(load_record(&env).is_empty());
        assert!(!lower(&env, "cargo-registry").join("cache").exists());
    }

    #[test]
    fn a_different_file_in_the_shared_layer_is_never_replaced() {
        let t = tempfile::tempdir().unwrap();
        let (env, proj) = project(t.path(), "npm");
        let data = b"package tarball".as_slice();
        let hex = crate::manifest::hex(&Sha512::digest(data));
        let rel = format!(
            "_cacache/content-v2/sha512/{}/{}/{}",
            &hex[..2],
            &hex[2..4],
            &hex[4..]
        );
        put(&upper(&env, &proj, "npm"), &rel, data);
        let low = lower(&env, "npm");
        put(&low, &rel, b"what was there");
        let r = run(&env, &proj, &Sources::default(), &http(), later());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].result, "refused: another file is in the shared layer");
        assert_eq!(std::fs::read(low.join(&rel)).unwrap(), b"what was there");

        // The same file there: nothing to do, nothing to audit.
        put(&low, &rel, data);
        assert!(run(&env, &proj, &Sources::default(), &http(), later()).is_empty());

        // place itself never replaces a file either.
        let root = Root::open(&low).unwrap();
        assert!(place(&root, Path::new(&rel), b"other").is_err());
        assert!(place(&root, Path::new(&rel), data).is_ok());
        assert_eq!(std::fs::read(low.join(&rel)).unwrap(), data);
    }

    #[test]
    fn npm_content_matches_the_hash_in_its_own_path() {
        let t = tempfile::tempdir().unwrap();
        let (env, proj) = project(t.path(), "npm");
        let up = upper(&env, &proj, "npm");
        let good = b"npm tarball".as_slice();
        let h512 = crate::manifest::hex(&Sha512::digest(good));
        let h256 = sha(good);
        let split = |h: &str| format!("{}/{}/{}", &h[..2], &h[2..4], &h[4..]);
        put(
            &up,
            &format!("_cacache/content-v2/sha512/{}", split(&h512)),
            good,
        );
        put(
            &up,
            &format!("_cacache/content-v2/sha256/{}", split(&h256)),
            good,
        );
        // Tampered: the content does not give the hash of its path.
        let other = crate::manifest::hex(&Sha512::digest(b"x"));
        put(
            &up,
            &format!("_cacache/content-v2/sha512/{}", split(&other)),
            b"planted",
        );
        put(&up, "_cacache/content-v2/sha1/ab/cd/ef", b"weak");
        // Not content: the index stays per project.
        put(&up, "_cacache/index-v5/ab/cd/entry", b"{}");
        let mut r = run(&env, &proj, &Sources::default(), &http(), later());
        r.sort_by(|a, b| a.package.cmp(&b.package));
        let res: Vec<_> = r
            .iter()
            .map(|o| (o.package.clone(), o.result.clone()))
            .collect();
        let mut want = vec![
            (
                "sha1:abcdef".to_string(),
                "refused: a weak or unknown hash".to_string(),
            ),
            (format!("sha256:{h256}"), "promoted".into()),
            (format!("sha512:{h512}"), "promoted".into()),
            (
                format!("sha512:{other}"),
                "refused: the content does not match its hash".into(),
            ),
        ];
        want.sort();
        assert_eq!(res, want);
        let low = lower(&env, "npm");
        assert!(low
            .join("_cacache/content-v2/sha512")
            .join(split(&h512))
            .is_file());
        assert!(!low
            .join("_cacache/content-v2/sha512")
            .join(split(&other))
            .exists());
        assert!(!low.join("_cacache/index-v5").exists());
    }

    // The fixture module.zip and the two h1: values come from Python, not
    // from this code:
    //
    //     import base64, hashlib, zipfile
    //     gomod = b"module example.com/m\n\ngo 1.21\n"
    //     files = {
    //         "example.com/m@v1.0.0/go.mod": gomod,
    //         "example.com/m@v1.0.0/m.go": b"package m\n\n"
    //             + b"// a line of the paddock test module\n" * 40,
    //     }
    //     with zipfile.ZipFile("module.zip", "w", zipfile.ZIP_DEFLATED) as z:
    //         for n, d in files.items():
    //             z.writestr(n, d)
    //     def h1(fs):
    //         s = "".join(f"{hashlib.sha256(d).hexdigest()}  {n}\n"
    //                     for n, d in sorted(fs.items()))
    //         return "h1:" + base64.b64encode(
    //             hashlib.sha256(s.encode()).digest()).decode()
    //     print(h1(files))
    //     print(h1({"go.mod": gomod}))
    const MODULE_ZIP: &[u8] = include_bytes!("module.zip");
    const ZIP_H1: &str = "h1:des50hXQr4TV9S1Yg91mrhSgmkSoE52zp3RQpauBq4E=";
    const MOD_H1: &str = "h1:ONeDgCa5UF/jJRjGzpOKmUiezgFEk4IPFZ96frvroW0=";
    const GO_MOD: &[u8] = b"module example.com/m\n\ngo 1.21\n";

    #[test]
    fn go_hash1_matches_independent_values() {
        let files = super::super::zip::files(MODULE_ZIP).unwrap();
        let names: Vec<_> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            ["example.com/m@v1.0.0/go.mod", "example.com/m@v1.0.0/m.go"]
        );
        assert_eq!(files[1].1.len(), 11 + 37 * 40);
        assert_eq!(hash1(&files).unwrap(), ZIP_H1);
        assert_eq!(
            hash1(&[("go.mod".to_string(), GO_MOD.to_vec())]).unwrap(),
            MOD_H1
        );
        assert_eq!(hash1(&[("a\nb".to_string(), vec![])]), None);
    }

    #[test]
    fn the_zip_reader_refuses_broken_input() {
        assert!(super::super::zip::files(b"").is_err());
        assert!(super::super::zip::files(&MODULE_ZIP[..MODULE_ZIP.len() - 30]).is_err());
        let mut z = MODULE_ZIP.to_vec();
        // The deflated data of the first entry starts after its local header.
        let at = 30 + "example.com/m@v1.0.0/go.mod".len();
        z[at] ^= 0xff;
        z[at + 1] ^= 0xff;
        assert!(super::super::zip::files(&z).is_err());
    }

    #[test]
    fn go_case_escaping_and_the_lookup_answer() {
        assert_eq!(
            escape("github.com/BurntSushi/toml"),
            "github.com/!burnt!sushi/toml"
        );
        assert_eq!(
            unescape("github.com/!burnt!sushi/toml").as_deref(),
            Some("github.com/BurntSushi/toml")
        );
        assert_eq!(unescape("Upper"), None);
        assert_eq!(unescape("end!"), None);
        assert_eq!(unescape("!1"), None);
        let body = format!(
            "1234\nexample.com/m v1.0.0 {ZIP_H1}\nexample.com/m v1.0.0/go.mod {MOD_H1}\n\ngo.sum database tree\n"
        );
        assert_eq!(
            sumdb_hash(&body, "example.com/m", "v1.0.0", false).as_deref(),
            Some(ZIP_H1)
        );
        assert_eq!(
            sumdb_hash(&body, "example.com/m", "v1.0.0", true).as_deref(),
            Some(MOD_H1)
        );
        assert_eq!(sumdb_hash(&body, "example.com/n", "v1.0.0", false), None);
    }

    #[test]
    fn go_modules_match_the_checksum_database() {
        let t = tempfile::tempdir().unwrap();
        let (env, proj) = project(t.path(), "go-mod");
        let up = upper(&env, &proj, "go-mod");
        let v = "cache/download/example.com/m/@v";
        put(&up, &format!("{v}/v1.0.0.zip"), MODULE_ZIP);
        put(&up, &format!("{v}/v1.0.0.mod"), GO_MOD);
        // Tampered: another go.mod for the same version.
        let w = "cache/download/example.com/!m/@v";
        put(&up, &format!("{w}/v1.0.0.mod"), b"module example.com/M\n");
        put(&up, &format!("{v}/list"), b"v1.0.0\n");
        let srv = serve(HashMap::from([
            (
                "/lookup/example.com/m@v1.0.0".to_string(),
                format!("1\nexample.com/m v1.0.0 {ZIP_H1}\nexample.com/m v1.0.0/go.mod {MOD_H1}\n\nsig\n"),
            ),
            (
                "/lookup/example.com/!m@v1.0.0".to_string(),
                format!("2\nexample.com/M v1.0.0/go.mod {MOD_H1}\n\nsig\n"),
            ),
        ]));
        let src = Sources {
            crates_index: String::new(),
            sumdb: srv.base.clone(),
        };
        let mut r = run(&env, &proj, &src, &http(), later());
        r.sort_by(|a, b| (&a.package, a.result.len()).cmp(&(&b.package, b.result.len())));
        let res: Vec<_> = r
            .iter()
            .map(|o| (o.package.as_str(), o.version.as_str(), o.result.as_str()))
            .collect();
        assert_eq!(
            res,
            vec![
                (
                    "example.com/M",
                    "v1.0.0",
                    "refused: the h1 hash of sum.golang.org differs"
                ),
                ("example.com/m", "v1.0.0", "promoted"),
                ("example.com/m", "v1.0.0", "promoted"),
            ]
        );
        let low = lower(&env, "go-mod");
        assert_eq!(
            std::fs::read(low.join(v).join("v1.0.0.zip")).unwrap(),
            MODULE_ZIP
        );
        assert!(low.join(v).join("v1.0.0.mod").is_file());
        assert!(!low.join(v).join("list").exists());
        assert!(!low.join(w).exists());
    }

    #[test]
    fn promotion_follows_only_bwrap_cache_overlays() {
        let t = tempfile::tempdir().unwrap();
        let (env, proj) = project(t.path(), "cargo-registry");
        let mut spec = crate::spec::SandboxSpec::new(
            "t".into(),
            "sh",
            &proj,
            t.path().join("home"),
            Path::new("/home/u"),
            1000,
            1000,
            "u",
        );
        assert!(!wanted(&env, &spec, "bwrap"));
        spec.add_mount(
            mount_with(
                &env,
                &proj,
                "cargo-registry",
                "/home/u/.cargo/registry".into(),
                "t",
                true,
            )
            .unwrap(),
        );
        assert!(wanted(&env, &spec, "bwrap"));
        assert!(!wanted(&env, &spec, "docker"));
        assert!(!wanted(&env, &spec, "podman"));
        assert!(command(&env, &proj, Some("https://index.example.org".into()), None).is_err());
    }
}
