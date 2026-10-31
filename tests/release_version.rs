//! Every place that names the release names the version in Cargo.toml. The
//! release PR bumps them together; this test fails when one is left behind.

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn read(path: &str) -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/").to_string() + path).unwrap()
}

/// Every `vX.Y.Z` tag that a text names.
fn tags(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    for (i, _) in text.match_indices('v') {
        let prev = i.checked_sub(1).map(|j| b[j]);
        if prev.is_some_and(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        let rest = &text[i + 1..];
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let num = rest[..end].trim_end_matches('.');
        if num.split('.').count() == 3 && num.split('.').all(|p| !p.is_empty()) {
            out.push(num.to_string());
        }
    }
    out
}

#[test]
fn the_install_commands_name_the_current_release() {
    for file in ["README.md", "skills/paddock/SKILL.md"] {
        let text = read(file);
        assert!(
            text.contains(&format!("git clone --branch v{VERSION} ")),
            "{file} does not clone v{VERSION}"
        );
        let other: Vec<String> = tags(&text).into_iter().filter(|t| t != VERSION).collect();
        assert!(other.is_empty(), "{file} names other releases: {other:?}");
    }
    let readme = read("README.md");
    for asset in [
        format!("releases/download/v{VERSION}/paddock-{VERSION}-x86_64-linux.tar.gz"),
        format!("releases/download/v{VERSION}/paddock-{VERSION}-x86_64-linux.tar.gz.sha256"),
    ] {
        assert!(
            readme.contains(&asset),
            "README.md does not download {asset}"
        );
    }
}

#[test]
fn the_package_files_name_the_current_version() {
    let manifest: serde_json::Value = serde_json::from_str(&read("manifest.json")).unwrap();
    assert_eq!(manifest["version"], VERSION, "manifest.json");
    assert!(
        read("pkgbuild/PKGBUILD").contains(&format!("\npkgver={VERSION}\n")),
        "pkgbuild/PKGBUILD"
    );
    assert!(
        read(".github/ISSUE_TEMPLATE/bug_report.yml").contains(&format!("paddock {VERSION}")),
        "bug_report.yml"
    );
}
