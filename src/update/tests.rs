use super::*;
use std::collections::BTreeMap;
use std::sync::Mutex;

pub(crate) fn env_in(t: &Path, sandbox: bool) -> HostEnv {
    let mut vars = BTreeMap::new();
    for (k, d) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "run"),
    ] {
        vars.insert(k.to_string(), t.join(d).display().to_string());
    }
    if sandbox {
        vars.insert("PADDOCK".into(), "1".into());
    }
    HostEnv::from_parts(t.join("home"), "demo".into(), 1000, 1000, vars)
}

/// A release source that serves fixed bytes and records every request.
struct Fake {
    release: Release,
    files: BTreeMap<String, Vec<u8>>,
    calls: Mutex<Vec<String>>,
}

impl Fake {
    fn new(version: &str, tarball: Vec<u8>, sums: Vec<u8>) -> Self {
        let release = Release::for_tag(&format!("v{version}"), "x86_64").unwrap();
        let mut files = BTreeMap::new();
        files.insert(release.tarball_url(), tarball);
        files.insert(release.checksum_url(), sums);
        Self {
            release,
            files,
            calls: Mutex::new(vec![]),
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Source for Fake {
    fn latest(&self) -> Result<Release> {
        self.calls.lock().unwrap().push("latest".into());
        Ok(self.release.clone())
    }
    fn download(&self, url: &str) -> Result<Vec<u8>> {
        self.calls.lock().unwrap().push(url.into());
        self.files.get(url).cloned().context("not found")
    }
}

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// A release tarball whose `paddock` is a script that prints `prints`.
fn tarball(t: &Path, version: &str, prints: &str) -> Vec<u8> {
    let name = format!("paddock-{version}-x86_64-linux");
    let dir = t.join("build").join(&name);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("paddock");
    std::fs::write(&bin, format!("#!/bin/sh\necho '{prints}'\n")).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = t.join(format!("{name}.tar.gz"));
    let st = Command::new("tar")
        .arg("-C")
        .arg(t.join("build"))
        .arg("-czf")
        .arg(&out)
        .arg(&name)
        .status()
        .unwrap();
    assert!(st.success());
    std::fs::read(out).unwrap()
}

fn sums_for(rel: &Release, data: &[u8]) -> Vec<u8> {
    format!("{}  {}\n", sha(data), rel.tarball()).into_bytes()
}

fn tar() -> PathBuf {
    PathBuf::from("tar")
}

fn old_binary(t: &Path) -> PathBuf {
    let bin = t.join("bin/paddock");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, "old").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

#[test]
fn versions_compare_like_semver() {
    assert!(is_newer("0.1.8", "0.1.7"));
    assert!(is_newer("v0.2.0", "0.1.17"));
    assert!(is_newer("0.1.10", "0.1.9"));
    assert!(is_newer("1.0.0", "0.99.99"));
    assert!(!is_newer("0.1.7", "0.1.7"));
    assert!(!is_newer("0.1.6", "0.1.7"));
    // A release is newer than its pre-releases, and pre-releases sort.
    assert!(is_newer("0.2.0", "0.2.0-rc.1"));
    assert!(!is_newer("0.2.0-rc.1", "0.2.0"));
    assert!(is_newer("0.2.0-rc.2", "0.2.0-rc.1"));
    assert!(is_newer("0.2.0-rc.10", "0.2.0-rc.9"));
    assert!(is_newer("0.2.0-rc.1", "0.1.9"));
    // Garbage is never newer.
    for bad in [
        "",
        "latest",
        "0.1",
        "0.1.7.1",
        "0.1.x",
        "0.1.7-",
        "0.1.7-a/b",
        "../0.1.7",
        "0.1.7+x",
    ] {
        assert!(Version::parse(bad).is_none(), "{bad}");
        assert!(!is_newer(bad, "0.0.1"), "{bad}");
    }
    assert_eq!(
        Version::parse("v1.2.3-beta.1").unwrap().to_string(),
        "1.2.3-beta.1"
    );
}

#[test]
fn release_names_follow_the_release_workflow() {
    let r = Release::for_tag("v0.1.8", "x86_64").unwrap();
    assert_eq!(r.tarball(), "paddock-0.1.8-x86_64-linux.tar.gz");
    assert_eq!(r.checksum(), "paddock-0.1.8-x86_64-linux.tar.gz.sha256");
    assert_eq!(r.member(), "paddock-0.1.8-x86_64-linux/paddock");
    assert_eq!(
        r.tarball_url(),
        "https://github.com/harnessix/omarchy-paddock/releases/download/v0.1.8/paddock-0.1.8-x86_64-linux.tar.gz"
    );
    assert!(check_url(&r.tarball_url()).is_ok());
    assert!(Release::for_tag("v0.1.8/../../x", "x86_64").is_err());
    assert!(Release::for_tag("nightly", "x86_64").is_err());
}

#[test]
fn aarch64_release_names_follow_the_release_workflow() {
    let r = Release::for_tag("v0.1.8", "aarch64").unwrap();
    assert_eq!(r.arch, "aarch64");
    assert_eq!(r.tarball(), "paddock-0.1.8-aarch64-linux.tar.gz");
    assert_eq!(r.checksum(), "paddock-0.1.8-aarch64-linux.tar.gz.sha256");
    assert_eq!(r.member(), "paddock-0.1.8-aarch64-linux/paddock");
    assert_eq!(
        r.tarball_url(),
        "https://github.com/harnessix/omarchy-paddock/releases/download/v0.1.8/paddock-0.1.8-aarch64-linux.tar.gz"
    );
    assert_eq!(
        r.checksum_url(),
        "https://github.com/harnessix/omarchy-paddock/releases/download/v0.1.8/paddock-0.1.8-aarch64-linux.tar.gz.sha256"
    );
    assert!(check_url(&r.checksum_url()).is_ok());
}

#[test]
fn other_architectures_have_no_release() {
    for arch in ["riscv64", "x86", "arm", "powerpc64", "", "x86_64 "] {
        let e = Release::for_tag("v0.1.8", arch).unwrap_err().to_string();
        assert!(e.contains("no release binary"), "{arch}: {e}");
        assert!(e.contains("x86_64 and aarch64"), "{arch}: {e}");
    }
    assert_eq!(release_arch("x86_64").unwrap(), "x86_64");
    assert_eq!(release_arch("aarch64").unwrap(), "aarch64");
}

/// The release workflow builds every architecture in [`ARCHS`] with the
/// asset names that [`Release`] downloads.
#[test]
fn release_workflow_builds_every_architecture() {
    let yml = include_str!("../../.github/workflows/release.yml");
    for arch in ARCHS {
        assert!(
            yml.contains(&format!("arch: {arch}")),
            "release.yml has no matrix entry for {arch}"
        );
    }
    assert!(yml.contains(r#"name="paddock-${TAG#v}-${{ matrix.arch }}-linux""#));
}

#[test]
fn only_github_hosts_over_https_are_allowed() {
    for ok in [
        "https://api.github.com/repos/harnessix/omarchy-paddock/releases/latest",
        "https://github.com/harnessix/omarchy-paddock/releases/download/v0.1.8/x.tar.gz",
        "https://objects.githubusercontent.com/github-production-release-asset-2e65be/1?X-Amz=1",
        "https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sp=r",
        "https://github.com:443/x",
        "https://GitHub.com/x",
    ] {
        assert!(check_url(ok).is_ok(), "{ok}");
    }
    for bad in [
        "http://github.com/x",
        "https://github.com.evil.example/x",
        "https://evil.example/github.com",
        "https://raw.githubusercontent.com/x",
        "https://example.githubusercontent.com/x",
        "https://user:pw@github.com/x",
        "https://github.com:8443/x",
        "https://127.0.0.1/x",
        "file:///etc/passwd",
        "ftp://github.com/x",
    ] {
        assert!(check_url(bad).is_err(), "{bad}");
    }
    // A refused redirect does not print the signed query.
    let e = check_url("https://evil.example/a?token=secret").unwrap_err();
    assert!(!e.to_string().contains("secret"), "{e}");
}

#[test]
fn responses_need_their_whole_body() {
    let r = parse_response(
        b"HTTP/1.1 302 Found\r\nLocation: https://github.com/x\r\nContent-Length: 0\r\n\r\n",
        10,
        false,
    )
    .unwrap();
    assert_eq!(r.status, 302);
    assert_eq!(r.location.as_deref(), Some("https://github.com/x"));
    let r = parse_response(
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
        10,
        false,
    )
    .unwrap();
    assert_eq!(r.body, b"hello");
    let r = parse_response(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n1;x=y\r\n!\r\n0\r\n\r\n",
        10,
        false,
    )
    .unwrap();
    assert_eq!(r.body, b"hello!");
    // Cut short: an error, never a shorter file.
    assert!(parse_response(
        b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nhello",
        10,
        true
    )
    .is_err());
    assert!(parse_response(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n",
        10,
        true
    )
    .is_err());
    assert!(parse_response(b"HTTP/1.1 200 OK\r\n\r\nhello", 10, false).is_err());
    assert!(parse_response(b"HTTP/1.1 200 OK\r\n\r\nhello", 10, true).is_ok());
    // Too large.
    assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\n", 10, true).is_err());
}

#[test]
fn checksum_mismatch_is_refused() {
    let data = b"tarball bytes";
    let name = "paddock-0.1.8-x86_64-linux.tar.gz";
    let good = format!("{}  {name}\n", sha(data));
    verify_checksum(data, good.as_bytes(), name).unwrap();
    verify_checksum(data, format!("{} *{name}\n", sha(data)).as_bytes(), name).unwrap();
    let e = verify_checksum(b"other bytes", good.as_bytes(), name).unwrap_err();
    assert!(e.to_string().contains("does not match"), "{e}");
    assert!(verify_checksum(
        data,
        format!("{}  other.tar.gz\n", sha(data)).as_bytes(),
        name
    )
    .is_err());
    assert!(verify_checksum(data, format!("abc  {name}\n").as_bytes(), name).is_err());
    assert!(verify_checksum(data, b"", name).is_err());
}

#[test]
fn a_bad_checksum_changes_nothing() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path(), false);
    let tb = tarball(t.path(), "9.9.9", "paddock 9.9.9");
    let src = Fake::new(
        "9.9.9",
        tb,
        format!("{}  paddock-9.9.9-x86_64-linux.tar.gz\n", sha(b"x")).into_bytes(),
    );
    let bin = old_binary(t.path());
    let e = install(&env, &src, &src.release, &bin, None, &tar()).unwrap_err();
    assert!(e.to_string().contains("does not match"), "{e}");
    assert_eq!(std::fs::read(&bin).unwrap(), b"old");
    // The private download folder is gone.
    let dl = env.paddock_cache().join("update");
    assert_eq!(std::fs::read_dir(&dl).map(|d| d.count()).unwrap_or(0), 0);
}

#[test]
fn update_replaces_the_binary_after_every_check() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path(), false);
    let tb = tarball(t.path(), "9.9.9", "paddock 9.9.9");
    let rel = Release::for_tag("v9.9.9", "x86_64").unwrap();
    let src = Fake::new("9.9.9", tb.clone(), sums_for(&rel, &tb));
    let bin = old_binary(t.path());
    install(&env, &src, &rel, &bin, None, &tar()).unwrap();
    let new = std::fs::read_to_string(&bin).unwrap();
    assert!(new.contains("echo 'paddock 9.9.9'"), "{new}");
    assert_eq!(
        std::fs::metadata(&bin).unwrap().permissions().mode() & 0o777,
        0o755
    );
    // No temporary file stays next to it.
    let left: Vec<_> = std::fs::read_dir(bin.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(left.len(), 1, "{left:?}");
}

#[test]
fn a_binary_with_the_wrong_version_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path(), false);
    let tb = tarball(t.path(), "9.9.9", "paddock 0.0.1");
    let rel = Release::for_tag("v9.9.9", "x86_64").unwrap();
    let src = Fake::new("9.9.9", tb.clone(), sums_for(&rel, &tb));
    let bin = old_binary(t.path());
    let e = install(&env, &src, &rel, &bin, None, &tar()).unwrap_err();
    assert!(e.to_string().contains("--version"), "{e}");
    assert_eq!(std::fs::read(&bin).unwrap(), b"old");
}

#[test]
fn update_refuses_inside_a_sandbox() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path(), true);
    let src = Fake::new("9.9.9", vec![], vec![]);
    let bin = old_binary(t.path());
    let e = install(&env, &src, &src.release, &bin, None, &tar()).unwrap_err();
    assert!(e.to_string().contains("host only"), "{e}");
    assert!(src.calls().is_empty());
    assert_eq!(std::fs::read(&bin).unwrap(), b"old");
    assert!(refuse_in_sandbox(&env, "paddock update").is_err());
    // No network from doctor either.
    assert_eq!(
        doctor_lines(&env)[0],
        "  skipped  not checked inside a sandbox"
    );
}

#[test]
fn update_refuses_a_package_owned_binary() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path(), false);
    let src = Fake::new("9.9.9", vec![], vec![]);
    let bin = old_binary(t.path());
    let e = install(
        &env,
        &src,
        &src.release,
        &bin,
        Some("omarchy-paddock 0.1.7-1"),
        &tar(),
    )
    .unwrap_err();
    let msg = e.to_string();
    assert!(
        msg.contains("omarchy-paddock 0.1.7-1")
            && msg.contains("bin/install")
            && msg.contains("pacman"),
        "{msg}"
    );
    assert!(src.calls().is_empty(), "nothing is downloaded");
    assert_eq!(std::fs::read(&bin).unwrap(), b"old");
    // Without pacman on PATH, a file in /usr still counts as a package's.
    assert!(package_owner(&env, Path::new("/usr/bin/paddock")).is_some());
    assert!(package_owner(&env, &bin).is_none());
}

#[test]
fn the_daily_check_uses_its_cache() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path(), false);
    let now = 1_800_000_000;
    // First run: fetches and caches.
    let v = latest_cached(&env, now, Duration::from_secs(5), || Ok("0.9.0".into()));
    assert_eq!(v.as_deref(), Some("0.9.0"));
    // Within a day: the cache answers and no fetch runs.
    let v = latest_cached(&env, now + 3600, Duration::from_secs(5), || {
        panic!("fetched")
    });
    assert_eq!(v.as_deref(), Some("0.9.0"));
    // A day later, a slow source does not hold doctor up.
    let start = Instant::now();
    let v = latest_cached(&env, now + DAY + 1, Duration::from_millis(100), || {
        std::thread::sleep(Duration::from_secs(3));
        Ok("1.0.0".into())
    });
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(v.as_deref(), Some("0.9.0"));
    // A failed attempt counts as the check of the day.
    let v = latest_cached(&env, now + 2 * DAY + 2, Duration::from_secs(5), || {
        bail!("offline")
    });
    assert_eq!(v.as_deref(), Some("0.9.0"));
    let v = latest_cached(&env, now + 2 * DAY + 3, Duration::from_secs(5), || {
        panic!("fetched")
    });
    assert_eq!(v.as_deref(), Some("0.9.0"));
}

#[test]
fn no_update_check_turns_the_network_off() {
    let t = tempfile::tempdir().unwrap();
    let mut env = env_in(t.path(), false);
    env.vars
        .insert("PADDOCK_NO_UPDATE_CHECK".into(), "1".into());
    assert!(doctor_lines(&env)[0].contains("PADDOCK_NO_UPDATE_CHECK=1"));
    assert!(!cache_file(&env).exists());
}

#[test]
fn release_lines() {
    assert!(release_line(Some("0.2.0"), "0.1.7").contains("paddock 0.2.0 is out"));
    assert!(release_line(Some("0.1.7"), "0.1.7").contains("latest release"));
    assert!(release_line(None, "0.1.7").contains("could not check"));
}

#[test]
fn files_changed_after_the_start_mean_a_restart() {
    let t = tempfile::tempdir().unwrap();
    let f = t.path().join("app");
    std::fs::write(&f, "v1").unwrap();
    let now = crate::state::now();
    assert!(changed_after(std::slice::from_ref(&f), now - 60));
    assert!(!changed_after(std::slice::from_ref(&f), now + 60));
    assert!(!changed_after(&[t.path().join("missing")], 0));
}
