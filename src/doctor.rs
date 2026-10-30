//! `paddock doctor`: dependency report and the escape test suite.

use crate::backend;
use crate::build::{self, Options};
use crate::env::HostEnv;
use crate::manifest::AppBus;
use crate::policy::DENIED_HOME;
use crate::profile::{Harness, HarnessFactory, Profile};
use crate::run;
use crate::spec::{GuiMode, Network};
use anyhow::{bail, Result};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const PROBE: &str = include_str!("probe.sh");

/// The file a hostile project mise config tries to create in the shared installs.
const MISE_CANARY: &str = "paddock-probe-hook";

/// Sourced by mise as `[env] _.source` and run as hooks: it writes the canary
/// into every place the shared installs could be.
const MISE_HOOK: &str = r#"for d in "$MISE_SYSTEM_INSTALLS_DIR" "$MISE_DATA_DIR/installs" "$HOME/.local/share/paddock-mise/installs"; do
  [ -d "$d" ] && (: > "$d/paddock-probe-hook") 2>/dev/null
done
true
"#;

pub struct Dep {
    pub cmd: &'static str,
    /// Package names for Arch, Debian/Ubuntu and Fedora.
    pub pkg: [&'static str; 3],
    pub required: bool,
    pub why: &'static str,
}

/// Every host dependency, in one place. `bin/install`, the PKGBUILD and the
/// README list the same packages.
pub const DEPS: &[Dep] = &[
    Dep {
        cmd: "git",
        pkg: ["git", "git", "git"],
        required: true,
        why: "project and worktree detection",
    },
    Dep {
        cmd: "bwrap",
        pkg: ["bubblewrap", "bubblewrap", "bubblewrap"],
        required: true,
        why: "the default backend",
    },
    Dep {
        cmd: "pasta",
        pkg: ["passt", "passt", "passt"],
        required: true,
        why: "network mode 'egress' on the bwrap backend",
    },
    Dep {
        cmd: "ip",
        pkg: ["iproute2", "iproute2", "iproute"],
        required: true,
        why: "isolating host addresses in network mode 'egress'",
    },
    Dep {
        cmd: "weston",
        pkg: ["weston", "weston", "weston"],
        required: false,
        why: "displays for desktop projects (gui headless and window)",
    },
    Dep {
        cmd: "weston-screenshooter",
        pkg: ["weston", "weston", "weston"],
        required: false,
        why: "paddock-screenshot",
    },
    Dep {
        cmd: "Xwayland",
        pkg: ["xorg-xwayland", "xwayland", "xorg-x11-server-Xwayland"],
        required: false,
        why: "X11 apps on the private display",
    },
    Dep {
        cmd: "dbus-run-session",
        pkg: ["dbus", "dbus", "dbus-daemon"],
        required: false,
        why: "a private session bus for desktop apps",
    },
    Dep {
        cmd: "gnome-keyring-daemon",
        pkg: ["gnome-keyring", "gnome-keyring", "gnome-keyring"],
        required: false,
        why: "a private keyring for app sandboxes and harnesses that need one (copilot)",
    },
    Dep {
        cmd: "docker",
        pkg: ["docker", "docker.io", "moby-engine"],
        required: false,
        why: "the container backend",
    },
    Dep {
        cmd: "podman",
        pkg: ["podman", "podman", "podman"],
        required: false,
        why: "the rootless container backend",
    },
    Dep {
        cmd: "srt",
        pkg: [
            "sandbox-runtime (npm)",
            "sandbox-runtime (npm)",
            "sandbox-runtime (npm)",
        ],
        required: false,
        why: "the srt backend and network mode 'allowlist' (also needs socat and ripgrep)",
    },
    Dep {
        cmd: "socat",
        pkg: ["socat", "socat", "socat"],
        required: false,
        why: "the srt backend",
    },
    Dep {
        cmd: "rg",
        pkg: ["ripgrep", "ripgrep", "ripgrep"],
        required: false,
        why: "the srt backend",
    },
    Dep {
        cmd: "mise",
        pkg: ["mise", "mise", "mise"],
        required: false,
        why: "per-project toolchains from mise.toml",
    },
];

pub fn family() -> usize {
    let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let has = |w: &str| {
        text.lines()
            .any(|l| (l.starts_with("ID=") || l.starts_with("ID_LIKE=")) && l.contains(w))
    };
    if has("arch") {
        0
    } else if has("debian") || has("ubuntu") {
        1
    } else {
        2
    }
}

pub fn install_command(pkgs: &[&str]) -> String {
    let list = pkgs.join(" ");
    match family() {
        0 => format!("sudo pacman -S --needed {list}"),
        1 => format!("sudo apt install {list}"),
        _ => format!("sudo dnf install {list}"),
    }
}

pub fn report(env: &HostEnv) -> Result<bool> {
    let fam = family();
    let mut missing_required = Vec::new();
    let mut missing_optional = Vec::new();
    println!("Dependencies:");
    for d in DEPS {
        let found = env.which(d.cmd);
        let mark = match (&found, d.required) {
            (Some(_), _) => "ok      ",
            (None, true) => "MISSING ",
            (None, false) => "absent  ",
        };
        println!("  {mark} {:<22} {}", d.cmd, d.why);
        if found.is_none() {
            let list = if d.required {
                &mut missing_required
            } else {
                &mut missing_optional
            };
            if !list.contains(&d.pkg[fam]) {
                list.push(d.pkg[fam]);
            }
        }
    }
    if !missing_required.is_empty() {
        println!(
            "\nInstall the required packages:\n  {}",
            install_command(&missing_required)
        );
    }
    if !missing_optional.is_empty() {
        println!(
            "\nOptional packages:\n  {}",
            install_command(&missing_optional)
        );
    }

    println!("\nBackends:");
    let probe_spec = crate::spec::SandboxSpec::new(
        "doctor".into(),
        "doctor",
        &env.home,
        env.home.clone(),
        &env.home,
        env.uid,
        env.gid,
        &env.user,
    );
    for name in ["bwrap", "podman", "docker", "srt"] {
        let b = backend::by_name(name)?;
        // srt only filters by domain: check it with the mode it serves.
        let mut spec = probe_spec.clone();
        if name == "srt" {
            spec.network = Network::Allowlist;
        }
        match b.check(env, &spec) {
            Ok(()) => println!("  ok       {name}"),
            Err(e) => println!("  no       {name}: {e}"),
        }
    }

    println!("\nHarnesses:");
    let factory = HarnessFactory::new(env)?;
    for name in factory.names() {
        match factory.create(&name) {
            Ok(h) => {
                println!("  ok       {:<10} {}", name, h.exe.display());
                for u in &h.profile.unverified {
                    println!("           {:<10} not verified on a real install: {u}", "");
                }
            }
            Err(_) => println!("  absent   {name}"),
        }
    }

    println!("\nShell commands:");
    if crate::shims::installed(env) {
        let cmds = crate::shims::active(env);
        let home = env.home.display().to_string();
        let rcs: Vec<String> = crate::shims::loaded_from(env)
            .iter()
            .map(|p| p.display().to_string().replacen(&home, "~", 1))
            .collect();
        println!(
            "  on       these commands start in a sandbox: {}",
            cmds.join(", ")
        );
        println!("           loaded by one line in {}", rcs.join(" and "));
        if let Some(c) = cmds.iter().find(|c| *c == "claude").or(cmds.first()) {
            println!("           run one without the sandbox: PADDOCK_BYPASS=1 {c}");
        }
        println!("           turn all of them off: paddock shims remove");
    } else {
        println!("  off      harness commands run without a sandbox; paddock shims install makes the sandbox the default");
    }
    Ok(missing_required.is_empty())
}

/// The IPv4 default gateway, from /proc/net/route.
/// The id of the host session bus, if there is one.
fn host_bus_id() -> Option<String> {
    let c = zbus::blocking::Connection::session().ok()?;
    let id = zbus::blocking::fdo::DBusProxy::new(&c)
        .ok()?
        .get_id()
        .ok()?;
    Some(id.to_string())
}

fn default_gateway() -> Option<std::net::IpAddr> {
    let text = std::fs::read_to_string("/proc/net/route").ok()?;
    text.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() > 2 && f[1] == "00000000" {
            let gw = u32::from_str_radix(f[2], 16).ok()?;
            (gw != 0).then(|| std::net::IpAddr::from(gw.to_le_bytes()))
        } else {
            None
        }
    })
}

const KEYRINGS: &str = ".local/share/keyrings";

fn doctor_harness() -> Result<Harness> {
    // The doctor reads a project `.mcp.json`, as the claude profile does.
    let profile: Profile = toml::from_str(
        "name = \"doctor\"\n[[mcp]]\nfile = \".mcp.json\"\nin_project = true\npath = [\"mcpServers\"]\n",
    )?;
    Ok(Harness {
        profile,
        exe: PathBuf::from("/bin/sh"),
        install_mounts: vec![],
        path_dirs: vec![],
        source: "doctor".into(),
    })
}

pub struct EscapeOpts {
    pub backend: String,
    pub network: Network,
    pub display: GuiMode,
    /// Also check that DNS and HTTPS work. Needs network access.
    pub online: bool,
    /// Test the default (inherit) or the clean environment mode.
    pub inherit: bool,
    /// App mode: the session bus to test.
    pub app_bus: AppBus,
}

/// Run the probe in a real sandbox. Returns the number of failed checks.
pub fn escape(env: &HostEnv, o: &EscapeOpts) -> Result<usize> {
    let root = env.paddock_cache().join("doctor");
    clear_doctor_root(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project)?;

    // Host variables for the environment checks: one that inheritance passes,
    // a cloud credential, and two host control channels (one known by its
    // name, one by its value, a real socket).
    let ctl = root.join("ctl.sock");
    let _ctl = std::os::unix::net::UnixListener::bind(&ctl)?;
    std::env::set_var("DOCTOR_PLAIN_CANARY", "1");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "paddock-doctor-canary");
    std::env::set_var("GH_TOKEN", "paddock-doctor-canary");
    std::env::set_var("DOCTOR_CANARY_SOCKET", "/nonexistent");
    std::env::set_var("DOCTOR_CANARY_CTL", &ctl);
    let detected = HostEnv::detect()?;
    let env = &detected;
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(args)
            .status()
    };
    if !git(&["init", "-q", "-b", "main"])?.success() {
        bail!("git init failed");
    }
    // A local bare repository as `origin`, so the git-push checks need no
    // network. The sandbox cannot see it; only the host push reaches it.
    let remote = root.join("remote.git");
    let ident = [
        "-c",
        "user.name=paddock",
        "-c",
        "user.email=doctor@localhost",
    ];
    let ready = Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&remote)
        .status()?
        .success()
        && git(&[
            &ident[..],
            &["commit", "-q", "--allow-empty", "-m", "start"],
        ]
        .concat())?
        .success()
        && git(&["remote", "add", "origin", &remote.display().to_string()])?.success()
        && git(&["push", "-q", "origin", "main"])?.success();
    if !ready {
        bail!("cannot set up the git remote for the push checks");
    }
    std::fs::write(project.join(".env"), "TOKEN=paddock-secret-canary\n")?;
    std::fs::write(project.join("README"), "paddock doctor project\n")?;
    // A hostile mise config: its env script and hooks try to write into the
    // shared installs, which every project's sandbox executes. With network,
    // a plain registry tool must still install into the shared installs.
    let mise = env.which("mise").is_some();
    let shared_mise: Vec<PathBuf> = [crate::mise::SHARED_CACHE, "mise-installs"]
        .iter()
        .map(|d| env.paddock_cache().join("shared").join(d))
        .collect();
    if mise {
        std::fs::write(project.join(".paddock-probe-mise.sh"), MISE_HOOK)?;
        let tools = if o.online {
            "[tools]\njq = \"1.7.1\"\n"
        } else {
            ""
        };
        std::fs::write(
            project.join("mise.toml"),
            format!(
                "[env]\n_.source = \"./.paddock-probe-mise.sh\"\n\n[hooks]\nenter = \"sh ./.paddock-probe-mise.sh\"\npostinstall = \"sh ./.paddock-probe-mise.sh\"\n\n{tools}"
            ),
        )?;
        for d in &shared_mise {
            let _ = std::fs::remove_file(d.join(MISE_CANARY));
        }
    }
    let gui = o.display != GuiMode::Off;
    if gui {
        // A desktop marker, so the desktop detector path is exercised as well.
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"probe\"\n[dependencies]\nwinit = \"0\"\n",
        )?;
    }

    // A file in another sandbox home and one in the host home: both must stay invisible.
    let other = env
        .paddock_data()
        .join("homes")
        .join("doctor-other")
        .join("canary");
    std::fs::create_dir_all(other.parent().unwrap())?;
    std::fs::write(&other, "x")?;
    let host_file = root.join("host-canary");
    std::fs::write(&host_file, "x")?;
    // Another repository with a package in its node_modules and a `.env`.
    // The project `.mcp.json` names the package: without an approval, no
    // part of that repository may appear in the sandbox.
    let sibling = root.join("sibling");
    let pkg = sibling.join("node_modules/paddock-probe-mcp");
    std::fs::create_dir_all(&pkg)?;
    std::fs::write(sibling.join(".env"), "TOKEN=paddock-sibling-canary\n")?;
    let cli = pkg.join("cli.js");
    std::fs::write(&cli, "#!/bin/sh\nexit 0\n")?;
    std::fs::set_permissions(&cli, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    std::fs::write(
        project.join(".mcp.json"),
        serde_json::json!({ "mcpServers": { "probe": { "command": cli } } }).to_string(),
    )?;

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    // A service on every interface, to probe the host's other addresses.
    let any = TcpListener::bind("0.0.0.0:0")?;
    let any_port = any.local_addr()?.port();
    let local_addrs: Vec<String> = host_ipv4_addrs()
        .into_iter()
        .filter(|a| !a.starts_with("127."))
        .collect();

    // `.gitconfig` inside the sandbox is the identity-only file paddock writes.
    // On the private bus, the sandbox home holds the app's own keyring.
    let private_bus = o.display == GuiMode::App && o.app_bus == AppBus::Private;
    let mut denied: Vec<String> = DENIED_HOME
        .iter()
        .filter(|p| **p != ".gitconfig")
        .filter(|p| !(private_bus && **p == ".local/share/keyrings"))
        .map(|p| env.home.join(p).display().to_string())
        .collect();
    if let Some(rt) = &env.runtime_dir {
        // In app mode the same path holds the filtered bus proxy; the app
        // checks test that it reaches only the portals.
        if o.display != GuiMode::App {
            denied.push(rt.join("bus").display().to_string());
        }
        if let Some(w) = env.var("WAYLAND_DISPLAY") {
            denied.push(rt.join(w).display().to_string());
        }
    }

    let opts = Options {
        harness: "doctor".into(),
        project: Some(project.clone()),
        backend: Some(o.backend.clone()),
        network: Some(o.network),
        gui: Some(o.display),
        assume_yes: true,
        inherit_env: Some(o.inherit),
        args: vec!["-c".into(), PROBE.into()],
        app_bus: Some(o.app_bus),
        allow_domains: if o.network == Network::Allowlist {
            vec!["example.com".into()]
        } else {
            vec![]
        },
        ..Default::default()
    };
    let mut built = build::build_with(env, &opts, doctor_harness()?)?;
    crate::safefs::Root::open(&built.spec.home_src)?.write(
        Path::new(".paddock-doctor-canary"),
        b"x",
        0o600,
    )?;
    // The private bus test leaves its keyring in the doctor's home, which the
    // other tests share. Remove it before and after.
    // Through safefs: a planted .local link must not lead the removal out.
    let home_src = built.spec.home_src.clone();
    let _ = crate::safefs::Root::open(&home_src).and_then(|r| r.remove_tree(Path::new(KEYRINGS)));
    let s = &mut built.spec;
    s.set_env(
        "PADDOCK_PROBE_PROJECT",
        project.display().to_string(),
        "doctor",
    );
    s.set_env("PADDOCK_PROBE_DENIED", denied.join(":"), "doctor");
    // The default gateway is on the LAN on most machines; with the LAN
    // blocked, the sandbox must not reach it.
    if !s.lan {
        if let Some(gw) = default_gateway().filter(crate::proxy::is_lan) {
            s.set_env("PADDOCK_PROBE_LAN", gw.to_string(), "doctor");
        }
    }
    s.set_env("PADDOCK_PROBE_OTHER", other.display().to_string(), "doctor");
    s.set_env(
        "PADDOCK_PROBE_MCP_SIBLING",
        sibling.display().to_string(),
        "doctor",
    );
    s.set_env(
        "PADDOCK_PROBE_HOSTFILE",
        host_file.display().to_string(),
        "doctor",
    );
    s.set_env("PADDOCK_PROBE_PORT", port.to_string(), "doctor");
    s.set_env("PADDOCK_PROBE_ANYPORT", any_port.to_string(), "doctor");
    s.set_env("PADDOCK_PROBE_LOCAL_ADDRS", local_addrs.join(" "), "doctor");
    s.set_env(
        "PADDOCK_PROBE_HOSTPID",
        std::process::id().to_string(),
        "doctor",
    );
    s.set_env(
        "PADDOCK_PROBE_NETWORK",
        format!("{:?}", o.network).to_lowercase(),
        "doctor",
    );
    if o.display == GuiMode::App {
        s.set_env("PADDOCK_PROBE_APP", "1", "doctor");
        if private_bus {
            s.set_env("PADDOCK_PROBE_APP_BUS", "private", "doctor");
            // The probe compares it with the id of its own bus.
            if let Some(id) = host_bus_id() {
                s.set_env("PADDOCK_PROBE_HOST_BUS_ID", id, "doctor");
            }
        }
    } else if gui {
        s.set_env("PADDOCK_PROBE_GUI", "1", "doctor");
    }
    if o.online {
        s.set_env("PADDOCK_PROBE_ONLINE", "1", "doctor");
    }
    if mise {
        s.set_env("PADDOCK_PROBE_MISE", "1", "doctor");
    }
    s.set_env(
        "PADDOCK_PROBE_ENV",
        if o.inherit { "inherit" } else { "clean" },
        "doctor",
    );
    built.spec.set_env("PADDOCK_PROBE_PUSH", "1", "doctor");
    // The git escape checks: a path that a planted setting would create on
    // the host, and a directory that a planted pointer file would name.
    let git_canary = root.join("git-canary");
    let _ = std::fs::remove_file(&git_canary);
    built.spec.set_env(
        "PADDOCK_PROBE_GIT_CANARY",
        git_canary.display().to_string(),
        "doctor",
    );
    built.spec.set_env(
        "PADDOCK_PROBE_GIT_EVIL",
        root.join("evil.git").display().to_string(),
        "doctor",
    );
    // Host passthrough requests from the probe are denied, except pushes to
    // the known remote, which get "allow for this session".
    let recorder = std::sync::Arc::new(Recorder::default());
    built.approver = Some(recorder.clone());
    s_set_approvals(&mut built, env);
    let env2 = HostEnv::detect()?;
    // A file in the host /tmp: srt shares the host /tmp unless it is denied.
    let host_tmp = std::path::PathBuf::from(format!("/tmp/paddock-doctor-{}", std::process::id()));
    std::fs::write(&host_tmp, "x")?;
    let host_sock =
        std::path::PathBuf::from(format!("/tmp/paddock-doctor-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&host_sock);
    let _host_sock_l = std::os::unix::net::UnixListener::bind(&host_sock)?;
    built.spec.set_env(
        "PADDOCK_PROBE_HOSTSOCK",
        host_sock.display().to_string(),
        "doctor",
    );
    built.spec.set_env(
        "PADDOCK_PROBE_HOSTTMP",
        host_tmp.display().to_string(),
        "doctor",
    );
    let b = run::select_backend(&env2, &built)?;
    built
        .spec
        .set_env("PADDOCK_PROBE_BACKEND", b.name(), "doctor");
    if crate::backend::relay_socket(&env2, b.name(), &built.spec).is_some() {
        built.spec.set_env("PADDOCK_PROBE_RELAY", "1", "doctor");
    }
    println!(
        "Escape test: backend {}, network {:?}, env {}, display {}",
        b.name(),
        o.network,
        if o.inherit { "inherit" } else { "clean" },
        format!("{:?}", o.display).to_lowercase()
    );
    // A watchdog: a backend that hangs (for example a container daemon that
    // never starts the container) fails the test instead of blocking it.
    let id = built.spec.id.clone();
    let cli = b.name().to_string();
    let env3 = env2.clone();
    let timed_out = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = timed_out.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(300));
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        for r in crate::state::Running::list(&env3)
            .into_iter()
            .filter(|r| r.id == id)
        {
            unsafe { libc::kill(r.pid as i32, libc::SIGKILL) };
        }
        if cli != "bwrap" {
            let _ = Command::new(&cli)
                .args(["rm", "-f", &format!("paddock-{id}")])
                .output();
        }
    });
    let outside = built
        .spec
        .home_src
        .parent()
        .map(|d| d.join("paddock-escape-probe"));
    if let Some(o) = &outside {
        let _ = std::fs::remove_file(o);
    }
    // The probe plants links and a FIFO; the host must not follow them.
    let planted = Planted::before(&built.spec);
    let outcome = run::run(&env2, built, b.as_ref(), true, true)?;
    let _ = crate::safefs::Root::open(&home_src).and_then(|r| r.remove_tree(Path::new(KEYRINGS)));
    let _ = std::fs::remove_file(&host_tmp);
    let _ = std::fs::remove_file(&host_sock);
    if timed_out.load(std::sync::atomic::Ordering::SeqCst) {
        println!("  FAIL probe-finished: the sandbox did not finish in 300 seconds");
        return Ok(1);
    }
    drop(listener);
    drop(any);
    let _ = std::fs::remove_dir_all(other.parent().unwrap());

    let text = outcome.stdout.unwrap_or_default();
    let mut failed = 0;
    let mut seen = 0;
    for line in text.lines() {
        if line.starts_with("ok ") || line.starts_with("skip ") {
            seen += 1;
            println!("  {line}");
        } else if line.starts_with("FAIL ") {
            seen += 1;
            failed += 1;
            println!("  {line}");
        }
    }
    for (name, problem) in git_host_checks(&env2, &project, &git_canary) {
        seen += 1;
        match problem {
            None => println!("  ok {name}"),
            Some(p) => {
                failed += 1;
                println!("  FAIL {name}: {p}");
            }
        }
    }
    for (name, problem) in push_checks(&recorder, &remote) {
        seen += 1;
        match problem {
            None => println!("  ok {name}"),
            Some(p) => {
                failed += 1;
                println!("  FAIL {name}: {p}");
            }
        }
    }
    if mise {
        seen += 1;
        let reached: Vec<String> = shared_mise
            .iter()
            .map(|d| d.join(MISE_CANARY))
            .filter(|f| f.exists())
            .map(|f| f.display().to_string())
            .collect();
        if reached.is_empty() {
            println!("  ok mise-config-cannot-write-shared");
        } else {
            failed += 1;
            println!(
                "  FAIL mise-config-cannot-write-shared: a project mise hook wrote {}",
                reached.join(", ")
            );
            for f in reached {
                let _ = std::fs::remove_file(f);
            }
        }
    }
    match &outside {
        Some(o) if o.exists() => {
            println!(
                "  FAIL outside-home-not-persisted: a write next to HOME reached {}",
                o.display()
            );
            failed += 1;
            let _ = std::fs::remove_file(o);
        }
        _ => println!("  ok outside-home-not-persisted"),
    }
    for (name, problem) in planted.check(env) {
        seen += 1;
        match problem {
            None => println!("  ok {name}"),
            Some(p) => {
                failed += 1;
                println!("  FAIL {name}: {p}");
            }
        }
    }
    if seen == 0 {
        println!(
            "  FAIL probe-ran: the probe printed nothing (exit code {})",
            outcome.code
        );
        failed += 1;
    }
    clear_doctor_root(&root);
    println!(
        "{}",
        if failed == 0 {
            "All checks passed."
        } else {
            "Some checks failed."
        }
    );
    Ok(failed)
}

/// Things the probe leaves behind for the host, as a sandbox could between
/// runs: a FIFO and a `/dev/zero` link at files the host reads after exit,
/// and links in the home where a mount point could go.
struct Planted {
    project: PathBuf,
    home_src: PathBuf,
    home: PathBuf,
    watched: std::collections::BTreeMap<PathBuf, Option<String>>,
}

/// Names in the probe; keep them in step with probe.sh.
const PLANTED_FIFO: &str = ".envrc";
const PLANTED_ZERO: &str = ".vscode/tasks.json";
const PLANTED_DIR_LINK: &str = ".paddock-probe-dir-link";
const PLANTED_FILE_LINK: &str = ".paddock-probe-file-link";
const LINK_TARGET_DIR: &str = "paddock-escape-link-dir";
const LINK_TARGET_FILE: &str = "paddock-escape-link-file";

impl Planted {
    fn before(spec: &crate::spec::SandboxSpec) -> Self {
        let home_src = spec.home_src.clone();
        let r = crate::safefs::Root::open(&home_src);
        for n in [PLANTED_DIR_LINK, PLANTED_FILE_LINK] {
            if let Ok(r) = &r {
                let _ = r.remove_tree(Path::new(n));
            }
        }
        if let Some(p) = home_src.parent() {
            let _ = std::fs::remove_dir_all(p.join(LINK_TARGET_DIR));
            let _ = std::fs::remove_file(p.join(LINK_TARGET_FILE));
        }
        Self {
            project: spec.project.clone(),
            home_src,
            home: spec.home.clone(),
            watched: crate::state::snapshot(&spec.watch),
        }
    }

    fn check(&self, env: &HostEnv) -> Vec<(&'static str, Option<String>)> {
        let mut out = Vec::new();
        // The host compares watched files after exit. It must not hang on
        // the FIFO or read /dev/zero, and it must report both.
        let fifo = self.project.join(PLANTED_FIFO);
        let zero = self.project.join(PLANTED_ZERO);
        if kind_of(&fifo) == Some(crate::safefs::Kind::Other)
            && kind_of(&zero) == Some(crate::safefs::Kind::Symlink)
        {
            let (tx, rx) = std::sync::mpsc::channel();
            let w = self.watched.clone();
            std::thread::spawn(move || {
                let _ = tx.send(crate::state::changed(&w));
            });
            out.push((
                "host-reads-planted-files-safely",
                match rx.recv_timeout(std::time::Duration::from_secs(20)) {
                    Ok(c) if c.contains(&fifo) && c.contains(&zero) => None,
                    Ok(c) => Some(format!("not reported as changed: {c:?}")),
                    Err(_) => Some("the host read of a planted FIFO or /dev/zero link hung".into()),
                },
            ));
        } else {
            out.push((
                "host-reads-planted-files-safely",
                Some("the probe did not plant the FIFO and the link".into()),
            ));
        }
        let _ = std::fs::remove_file(&fifo);
        let _ = std::fs::remove_file(&zero);

        // The next container start makes mount points in the home. Links
        // the sandbox planted there must not lead it outside.
        let mut s = crate::spec::SandboxSpec::new(
            "probe".into(),
            "doctor",
            &self.project,
            self.home_src.clone(),
            &self.home,
            env.uid,
            env.gid,
            &env.user,
        );
        let links = [PLANTED_DIR_LINK, PLANTED_FILE_LINK]
            .iter()
            .all(|n| kind_of(&self.home_src.join(n)) == Some(crate::safefs::Kind::Symlink));
        // Only with the links in place: else this would make real folders.
        let refused = links
            && [
                crate::spec::Mount::tmpfs(self.home.join(PLANTED_DIR_LINK).join("x"), "doctor"),
                crate::spec::Mount::mask(self.home.join(PLANTED_FILE_LINK), false, "doctor"),
            ]
            .into_iter()
            .all(|m| {
                s.mounts = vec![m];
                crate::backend::docker::precreate_mount_points(&s).is_err()
            });
        let parent = self.home_src.parent().unwrap_or(Path::new("/nonexistent"));
        let escaped = [LINK_TARGET_DIR, LINK_TARGET_FILE]
            .iter()
            .map(|n| parent.join(n))
            .find(|p| std::fs::symlink_metadata(p).is_ok());
        out.push((
            "mount-points-never-follow-planted-links",
            match (links, refused, escaped) {
                (false, _, _) => Some("the probe did not plant the links".into()),
                (_, _, Some(p)) => Some(format!("created {}", p.display())),
                (_, false, _) => Some("a planted link was accepted as a mount point".into()),
                _ => None,
            },
        ));
        if let Ok(r) = crate::safefs::Root::open(&self.home_src) {
            for n in [PLANTED_DIR_LINK, PLANTED_FILE_LINK] {
                let _ = r.remove_tree(Path::new(n));
            }
        }
        let _ = std::fs::remove_dir_all(parent.join(LINK_TARGET_DIR));
        let _ = std::fs::remove_file(parent.join(LINK_TARGET_FILE));
        out
    }
}

fn kind_of(p: &Path) -> Option<crate::safefs::Kind> {
    use crate::safefs::Kind;
    use std::os::unix::fs::FileTypeExt;
    let t = std::fs::symlink_metadata(p).ok()?.file_type();
    Some(if t.is_symlink() {
        Kind::Symlink
    } else if t.is_dir() {
        Kind::Dir
    } else if t.is_file() {
        Kind::File
    } else if t.is_socket() {
        Kind::Socket
    } else {
        Kind::Other
    })
}

/// The escape test's approver: it records every request, answers pushes to
/// the known remote with "allow for this session" and denies the rest.
#[derive(Default)]
struct Recorder {
    seen: std::sync::Mutex<Vec<crate::broker::Pending>>,
}

impl crate::broker::Approver for Recorder {
    fn ask(
        &self,
        _: &HostEnv,
        p: &crate::broker::Pending,
        _: std::time::Duration,
    ) -> Option<crate::broker::Answer> {
        self.seen.lock().unwrap().push(p.clone());
        let known_push = p.capability == "git-push" && !p.summary.starts_with("NEW REMOTE");
        Some(if known_push {
            crate::broker::Answer::Session
        } else {
            crate::broker::Answer::Deny
        })
    }
}

/// Remove the doctor's own project tree. The probe leaves git repositories
/// there, and git makes its object directories read-only, so the folders get
/// their owner write bit back first. A leftover tree would break the next
/// test run.
fn clear_doctor_root(root: &Path) {
    fn writable(dir: &Path, depth: usize) {
        if depth > 12 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let Ok(m) = std::fs::symlink_metadata(dir) else {
            return;
        };
        if m.is_symlink() || !m.is_dir() {
            return;
        }
        let mode = m.permissions().mode();
        if mode & 0o700 != 0o700 {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode | 0o700));
        }
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            writable(&e.path(), depth + 1);
        }
    }
    writable(root, 0);
    if let Err(e) = std::fs::remove_dir_all(root) {
        if e.kind() != std::io::ErrorKind::NotFound {
            eprintln!("paddock: cannot clear {}: {e}", root.display());
        }
    }
}

/// Host-side git checks, run after the sandbox exits. The probe planted
/// settings and hooks that would run a command on the host. These calls are
/// the ones paddock makes after every run, so nothing may start.
fn git_host_checks(
    env: &HostEnv,
    project: &Path,
    canary: &Path,
) -> Vec<(&'static str, Option<String>)> {
    let check = |ok: bool, why: String| (!ok).then_some(why);
    let layout = crate::hostgit::Layout::discover(project);
    let mut out = vec![(
        "git-layout-still-trusted",
        match &layout {
            Ok(Some(l)) if l.work_tree == project => None,
            Ok(Some(l)) => Some(format!(
                "the project root moved to {}",
                l.work_tree.display()
            )),
            Ok(None) => Some("the project is no longer a repository".into()),
            Err(e) => Some(format!("{e:#}")),
        },
    )];
    // Everything paddock runs on the host after a sandbox exits.
    let mut status = None;
    if let Ok(Some(l)) = &layout {
        let _ = l.try_query(&["rev-parse", "HEAD"]);
        let _ = l.try_query(&["log", "--oneline", "-n", "5"]);
        let _ = l.try_query(&["status", "--porcelain"]);
        status = l.status(env, "all").ok().flatten();
    }
    for s in crate::recover::Session::all(env)
        .into_iter()
        .filter(|s| s.project == project)
        .take(2)
    {
        let _ = crate::recover::project_changes(env, &s);
    }
    out.push((
        "git-planted-config-never-runs",
        check(
            !canary.exists(),
            format!(
                "a host git call ran a setting the sandbox planted and created {}",
                canary.display()
            ),
        ),
    ));
    out.push((
        "git-worktree-state-still-readable",
        match status {
            Some(lines) if lines.iter().any(|l| l.contains("planted.txt")) => None,
            Some(_) if env.which("bwrap").is_none() => None,
            Some(lines) => Some(format!("the sandboxed status missed the change: {lines:?}")),
            None if env.which("bwrap").is_none() => None,
            None => Some("the sandboxed status gave no answer".into()),
        },
    ));
    let _ = std::fs::remove_file(canary);
    out
}

/// Host-side checks of what the probe's pushes asked for.
fn push_checks(r: &Recorder, remote: &std::path::Path) -> Vec<(&'static str, Option<String>)> {
    let seen = r.seen.lock().unwrap();
    let pushes: Vec<_> = seen.iter().filter(|p| p.capability == "git-push").collect();
    let plain: Vec<_> = pushes
        .iter()
        .filter(|p| p.summary == "git push origin paddock-probe")
        .collect();
    let forced: Vec<_> = pushes
        .iter()
        .filter(|p| p.summary == "git push origin paddock-probe (force)")
        .collect();
    let new_remote: Vec<_> = pushes
        .iter()
        .filter(|p| p.summary.starts_with("NEW REMOTE: git push sneaky"))
        .collect();
    let on_remote = Command::new("git")
        .arg("-C")
        .arg(remote)
        .args(["rev-parse", "--verify", "-q", "refs/heads/paddock-probe"])
        .output()
        .is_ok_and(|o| o.status.success());
    let check = |ok: bool, why: String| (!ok).then_some(why);
    vec![
        (
            "git-push-request",
            check(
                plain.len() == 1 && on_remote,
                format!(
                    "{} push requests for the probe branch (expected 1), branch on the remote: {on_remote}",
                    plain.len()
                ),
            ),
        ),
        (
            "git-push-card-lists-commits",
            check(
                plain.first().is_some_and(|p| p.detail.contains("paddock probe push")),
                "the card does not list the pushed commit".into(),
            ),
        ),
        (
            "git-push-force-not-session-granted",
            check(
                forced.len() == 2 && forced.iter().all(|p| !p.session_allowed),
                format!(
                    "{} force push requests (expected 2, none with a session grant)",
                    forced.len()
                ),
            ),
        ),
        (
            "git-push-new-remote-flagged",
            check(
                new_remote.len() == 1 && !new_remote[0].session_allowed,
                format!("{} flagged requests for the added remote (expected 1)", new_remote.len()),
            ),
        ),
    ]
}

fn host_ipv4_addrs() -> Vec<String> {
    let out = Command::new("ip")
        .args(["-4", "-o", "addr", "show"])
        .output();
    let Ok(out) = out else { return vec![] };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().skip_while(|w| *w != "inet").nth(1))
        .filter_map(|cidr| cidr.split('/').next())
        .map(str::to_string)
        .collect()
}

fn s_set_approvals(built: &mut build::Built, env: &HostEnv) {
    let dir = crate::broker::approvals_dir(env);
    built.spec.set_env(
        "PADDOCK_PROBE_APPROVALS",
        dir.display().to_string(),
        "doctor",
    );
}
