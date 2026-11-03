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
    /// Crate from the crates.io index, with every published split for this file.
    Crate {
        splits: Vec<(String, String)>,
    },
    /// npm content: the algorithm and the hex digest from the path.
    Content(String, String),
    GoZip {
        escaped: String,
    },
    GoMod {
        escaped: String,
    },
}

impl Check {
    fn kind(&self) -> &'static str {
        match self {
            Check::Crate { .. } => "crate",
            Check::Content(..) => "content",
            Check::GoZip { .. } => "zip",
            Check::GoMod { .. } => "mod",
        }
    }
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
    let key = format!(
        "{}|{}|{}|{}|{sha}",
        c.cache,
        c.check.kind(),
        c.package,
        c.version
    );
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

fn crate_cksum(
    package: &str,
    version: &str,
    sources: &Sources,
    fetch: &dyn Fetch,
    index_cache: &mut BTreeMap<String, Option<String>>,
) -> Result<Option<String>> {
    let url = format!(
        "{}/{}",
        sources.crates_index.trim_end_matches('/'),
        index_path(package).context("not a crate name")?
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
    Ok(entry.and_then(|e| index_cksum(&e, version)))
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
        Check::Crate { splits } => {
            let this_ck = crate_cksum(&c.package, &c.version, sources, fetch, index_cache)?;
            let Some(ck) = this_ck else {
                return Ok("the crates.io index has no such version".into());
            };
            if !ck.eq_ignore_ascii_case(sha) {
                return Ok("the cksum of the crates.io index differs".into());
            }
            for (pkg, ver) in splits {
                if pkg == &c.package && ver == &c.version {
                    continue;
                }
                if let Some(other_ck) = crate_cksum(pkg, ver, sources, fetch, index_cache)? {
                    if !other_ck.eq_ignore_ascii_case(sha) {
                        return Ok("another published split has a different cksum".into());
                    }
                }
            }
            "ok".into()
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
                    let splits = crate_splits(stem);
                    for (package, version) in &splits {
                        out.push(Candidate {
                            cache,
                            rel: d.join(&f),
                            package: package.clone(),
                            version: version.clone(),
                            check: Check::Crate {
                                splits: splits.clone(),
                            },
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
mod tests;
