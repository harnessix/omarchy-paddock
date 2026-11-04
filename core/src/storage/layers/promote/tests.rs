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
const MODULE_ZIP: &[u8] = include_bytes!("../module.zip");
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
            format!(
                "1\nexample.com/m v1.0.0 {ZIP_H1}\nexample.com/m v1.0.0/go.mod {MOD_H1}\n\nsig\n"
            ),
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
fn the_same_bytes_are_not_promoted_as_both_a_mod_and_a_zip() {
    let t = tempfile::tempdir().unwrap();
    let (env, proj) = project(t.path(), "go-mod");
    let up = upper(&env, &proj, "go-mod");
    let v = "cache/download/example.com/m/@v";
    put(&up, &format!("{v}/v1.0.0.mod"), GO_MOD);
    put(&up, &format!("{v}/v1.0.0.zip"), GO_MOD);
    let srv = serve(HashMap::from([(
        "/lookup/example.com/m@v1.0.0".to_string(),
        format!("1\nexample.com/m v1.0.0 {ZIP_H1}\nexample.com/m v1.0.0/go.mod {MOD_H1}\n\nsig\n"),
    )]));
    let src = Sources {
        crates_index: String::new(),
        sumdb: srv.base.clone(),
    };
    let r = run(&env, &proj, &src, &http(), later());
    let zip = lower(&env, "go-mod").join(v).join("v1.0.0.zip");
    assert!(
        !zip.exists(),
        "shared zip is the go.mod bytes, which are not {ZIP_H1}: {r:?} bytes={:?}",
        std::fs::read(&zip).ok()
    );
}

#[test]
fn a_zip_with_a_bad_crc_stays_in_the_project() {
    let mut bad = MODULE_ZIP.to_vec();
    let min = bad.len().saturating_sub(22 + 65535);
    let eocd = (min..bad.len().saturating_sub(21))
        .rev()
        .find(|&i| bad[i..].starts_with(&[0x50, 0x4b, 0x05, 0x06]))
        .unwrap();
    let cd = u32::from_le_bytes(bad[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
    assert_eq!(&bad[cd..cd + 4], b"PK\x01\x02");
    bad[cd + 16] ^= 0xff;
    assert!(super::super::zip::files(&bad).is_err());
    let t = tempfile::tempdir().unwrap();
    let (env, proj) = project(t.path(), "go-mod");
    let up = upper(&env, &proj, "go-mod");
    let v = "cache/download/example.com/m/@v";
    put(&up, &format!("{v}/v1.0.0.zip"), &bad);
    let srv = serve(HashMap::from([(
        "/lookup/example.com/m@v1.0.0".to_string(),
        format!("1\nexample.com/m v1.0.0 {ZIP_H1}\nexample.com/m v1.0.0/go.mod {MOD_H1}\n\nsig\n"),
    )]));
    let src = Sources {
        crates_index: String::new(),
        sumdb: srv.base.clone(),
    };
    let r = run(&env, &proj, &src, &http(), later());
    let zip = lower(&env, "go-mod").join(v).join("v1.0.0.zip");
    assert!(
        !zip.exists(),
        "promoted a zip a strict reader rejects on CRC: {r:?}"
    );
}

#[test]
fn an_ambiguous_crate_name_is_not_promoted_on_one_cksum() {
    let t = tempfile::tempdir().unwrap();
    let (env, proj) = project(t.path(), "cargo-registry");
    let up = upper(&env, &proj, "cargo-registry");
    let dir = "cache/index.crates.io-1949cf8c6b5b557f";
    let body = b"one file, two names";
    put(&up, &format!("{dir}/a-1-2.0.0.crate"), body);
    let srv = serve(HashMap::from([
        (
            "/1/a".to_string(),
            format!(
                "{{\"name\":\"a\",\"vers\":\"1-2.0.0\",\"cksum\":\"{}\"}}\n",
                sha(body)
            ),
        ),
        (
            "/3/a/a-1".to_string(),
            format!(
                "{{\"name\":\"a-1\",\"vers\":\"2.0.0\",\"cksum\":\"{}\"}}\n",
                sha(b"the other crate")
            ),
        ),
    ]));
    let src = Sources {
        crates_index: srv.base.clone(),
        sumdb: String::new(),
    };
    let r = run(&env, &proj, &src, &http(), later());
    let placed = lower(&env, "cargo-registry")
        .join(dir)
        .join("a-1-2.0.0.crate");
    assert!(
        !placed.exists(),
        "promoted a file whose other identity has a different cksum: {r:?}"
    );
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
        Path::new("/home/demo"),
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
            "/home/demo/.cargo/registry".into(),
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
