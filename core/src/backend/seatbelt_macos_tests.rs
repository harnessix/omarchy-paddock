use super::*;
use std::process::Command;

fn run_in(profile: &str, cmd: &[&str]) -> bool {
    let t = tempfile::tempdir().unwrap();
    let f = t.path().join("p.sb");
    std::fs::write(&f, profile).unwrap();
    Command::new(SANDBOX_EXEC)
        .arg("-f")
        .arg(&f)
        .args(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[test]
fn ls_reads_the_project_but_not_the_real_home() {
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    // A folder in the real home, as most projects are.
    let base = tempfile::tempdir_in(&home).unwrap();
    let project = base.path().join("project");
    let secret = base.path().join("secret.txt");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("file.txt"), "x").unwrap();
    std::fs::write(project.join(".env"), "TOKEN=1").unwrap();
    std::fs::write(&secret, "secret").unwrap();
    let mut p = Policy::new(Net::None);
    p.add(real(&project), Access::ReadWrite, "project");
    p.add_file(real(&project.join(".env")), Access::Deny, "secrets");
    let sbpl = crate::seatbelt::render(&p).unwrap();
    let s = |p: &Path| p.display().to_string();
    assert!(run_in(&sbpl, &["/bin/ls", &s(&project)]));
    assert!(run_in(&sbpl, &["/bin/cat", &s(&project.join("file.txt"))]));
    assert!(!run_in(&sbpl, &["/bin/cat", &s(&secret)]));
    assert!(!run_in(&sbpl, &["/bin/cat", &s(&project.join(".env"))]));
    assert!(!run_in(&sbpl, &["/bin/ls", &s(&home)]));
    assert!(run_in(&sbpl, &["/usr/bin/touch", &s(&project.join("new"))]));
    assert!(!run_in(
        &sbpl,
        &["/usr/bin/touch", &s(&base.path().join("new"))]
    ));
}

#[test]
fn localhost_only_refuses_an_outside_address() {
    let l = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
    let port = l.local_addr().unwrap().port().to_string();
    std::thread::spawn(move || for _ in l.incoming() {});
    let nc = |sbpl: Option<&str>, host: &str| {
        let cmd = ["/usr/bin/nc", "-z", "-G", "2", host, &port];
        match sbpl {
            Some(p) => run_in(p, &cmd),
            None => Command::new(cmd[0])
                .args(&cmd[1..])
                .status()
                .unwrap()
                .success(),
        }
    };
    let local = crate::seatbelt::render(&Policy::new(Net::Localhost)).unwrap();
    let none = crate::seatbelt::render(&Policy::new(Net::None)).unwrap();
    assert!(nc(Some(&local), "127.0.0.1"));
    assert!(!nc(Some(&none), "127.0.0.1"));
    // An address of this Mac that is not loopback: the same listener
    // answers it outside the sandbox, so only the profile refuses it.
    let out = Command::new("/usr/sbin/ipconfig")
        .args(["getifaddr", "en0"])
        .output()
        .unwrap();
    let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if ip.is_empty() {
        eprintln!("no address on en0: the outside check is skipped");
        return;
    }
    assert!(nc(None, &ip));
    if nc(Some(&local), &ip) {
        // Some macOS versions (15) count the Mac's own addresses as
        // localhost. The port list must still hold there.
        eprintln!("Seatbelt counts {ip} as localhost on this macOS");
        let other = crate::seatbelt::render(&Policy::terminal(Net::Localhost, vec![9])).unwrap();
        assert!(!nc(Some(&other), &ip));
        assert!(!nc(Some(&other), "127.0.0.1"));
    }
}

#[test]
fn bundle_detection_on_this_mac() {
    // Only checks what is installed; it never starts an app. Apps change
    // their entitlements between versions, so codesign's own XML is the
    // reference, not a fixed answer per app.
    for b in [
        "/Applications/ChatGPT.app",
        "/Applications/Claude.app",
        "/Applications/Visual Studio Code.app",
    ] {
        let b = Path::new(b);
        if !b.is_dir() {
            continue;
        }
        let exe = bundle_exe(b).unwrap();
        let xml = std::process::Command::new("/usr/bin/codesign")
            .args(["-d", "--entitlements", "-", "--xml"])
            .arg(&exe)
            .stderr(std::process::Stdio::null())
            .output()
            .unwrap()
            .stdout;
        let flat: String = String::from_utf8_lossy(&xml)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let want = flat.contains("<key>com.apple.security.app-sandbox</key><true/>");
        assert_eq!(
            app_sandboxed(&entitlements(&exe).unwrap()),
            want,
            "{}",
            b.display()
        );
    }
}
