//! The repository must not contain facts about the machine it was written on.
//! The forbidden values are read from the environment at test time, so this
//! file itself contains none of them.

use std::process::Command;

fn forbidden() -> Vec<(String, String)> {
    let mut v = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        if home.len() > 5 {
            v.push(("home directory".into(), home));
        }
    }
    if let Ok(user) = std::env::var("USER") {
        if user.len() >= 4
            && !["root", "user", "runner", "build", "builder"].contains(&user.as_str())
        {
            v.push(("user name".into(), user));
        }
    }
    let host = std::fs::read_to_string("/etc/hostname").ok().or_else(|| {
        Command::new("uname")
            .arg("-n")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    });
    if let Some(h) = host {
        let h = h.trim().to_string();
        if h.len() >= 4 && h != "localhost" {
            v.push(("host name".into(), h));
        }
    }
    // The git server this checkout came from: an internal host name belongs in
    // the git remote configuration, not in tracked files.
    if let Ok(o) = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
    {
        let url = String::from_utf8_lossy(&o.stdout).trim().to_string();
        let host = url
            .split("://")
            .nth(1)
            .unwrap_or(&url)
            .split(['/', ':'])
            .next()
            .unwrap_or("")
            .rsplit('@')
            .next()
            .unwrap_or("")
            .to_string();
        if !host.is_empty()
            && !["github.com", "gitlab.com", "codeberg.org"].contains(&host.as_str())
        {
            v.push(("git server".into(), host));
        }
    }
    if let Ok(o) = Command::new("git").args(["config", "user.email"]).output() {
        let email = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if email.contains('@') {
            v.push(("git email".into(), email));
        }
    }
    v
}

#[test]
fn tracked_files_contain_no_local_data() {
    let root = env!("CARGO_MANIFEST_DIR");
    let out = Command::new("git")
        .args(["-C", root, "ls-files", "-co", "--exclude-standard"])
        .output()
        .expect("git ls-files");
    let files = String::from_utf8_lossy(&out.stdout);
    let needles = forbidden();
    let mut hits = Vec::new();
    for f in files.lines() {
        let Ok(text) = std::fs::read(format!("{root}/{f}")) else {
            continue;
        };
        let text = String::from_utf8_lossy(&text);
        for (what, needle) in &needles {
            if text.contains(needle.as_str()) {
                hits.push(format!("{f}: contains the {what}"));
            }
        }
    }
    assert!(hits.is_empty(), "local data found:\n{}", hits.join("\n"));
}
