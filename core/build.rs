//! Build paddock's pty library (src/ptyshim/shim.c) with the C compiler
//! that links Rust programs, and put it into `OUT_DIR`. paddock embeds it
//! (src/ptybroker.rs) and gives it to Seatbelt terminal runs on macOS. The
//! Linux build is for the tests, which load it with LD_PRELOAD.
//!
//! Without a C compiler, or when the target is another system, the file is
//! empty: the pty requests of a Seatbelt run then stay refused.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    release_repo();
    let src = "src/ptyshim/shim.c";
    println!("cargo:rerun-if-changed={src}");
    println!("cargo:rerun-if-env-changed=CC");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("paddock-pty.lib");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let host = std::env::var("HOST").unwrap_or_default();
    let target = std::env::var("TARGET").unwrap_or_default();
    let flags: &[&str] = match target_os.as_str() {
        "macos" => &["-dynamiclib"],
        "linux" => &["-shared", "-fPIC", "-ldl", "-lpthread"],
        _ => &[],
    };
    let built = !flags.is_empty()
        && host == target
        && Command::new(std::env::var("CC").unwrap_or_else(|_| "cc".into()))
            .args(["-O2", "-Wall", "-U_FORTIFY_SOURCE", "-o"])
            .arg(&out)
            .arg(src)
            .args(flags)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    if !built {
        if !flags.is_empty() {
            println!("cargo:warning=cannot build {src}: Seatbelt runs get no pty");
        }
        std::fs::write(&out, b"").unwrap();
    }
}

/// The GitHub repository whose releases `paddock update` and `paddock
/// doctor` read, as `OWNER/NAME`. A build for the releases of another
/// repository sets `PADDOCK_RELEASE_REPO`, as the release workflow and the
/// Homebrew formula do. src/update.rs reads the value.
fn release_repo() {
    println!("cargo:rerun-if-env-changed=PADDOCK_RELEASE_REPO");
    let repo = std::env::var("PADDOCK_RELEASE_REPO").unwrap_or_default();
    let repo = if repo.is_empty() {
        "harnessix/paddock".to_string()
    } else {
        repo
    };
    let part = |p: &str| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    match repo.split_once('/') {
        Some((owner, name)) if part(owner) && part(name) => {}
        _ => panic!("PADDOCK_RELEASE_REPO must be OWNER/NAME, not {repo:?}"),
    }
    println!("cargo:rustc-env=PADDOCK_RELEASE_REPO={repo}");
}
