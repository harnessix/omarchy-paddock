//! The workbench of a pasture: one terminal that every member and the user
//! can use, and that belongs to no member.
//!
//! A command in the terminal of a member runs in that member's sandbox and
//! can read its login. The workbench is a sandbox of its own instead: a
//! shell without a login, a harness or MCP servers, with an empty home,
//! the clean environment and no network. It joins the pasture, so it sees
//! the project, the shared folder `/pasture` and the shared ports, and it
//! can share ports too. It runs one tmux session.
//!
//! The tmux socket lives in a host folder of the pasture
//! (`pastures/<project hash>/<name>/workbench`). The workbench mounts it
//! read-write at `/run/paddock/workbench`; every member mounts it
//! read-only at the same place from its start, empty until the workbench
//! runs. A client can connect to a Unix socket on a read-only mount.
//!
//! `[pastures.NAME] workbench = true` allows it. The user starts it with
//! `paddock pasture workbench NAME --start`, or the first member that runs
//! `paddock-host pasture workbench` starts it. It stops with `--stop`,
//! when the user turns it off in the configuration, and when the last
//! member ends. The audit log gets each start, stop and join, never what
//! the terminal shows.

use crate::env::HostEnv;
use crate::state::Running;
use anyhow::{bail, Context as _, Result};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The harness name of the workbench sandbox. No member can use it.
pub const NAME: &str = "workbench";
/// Where the socket folder appears in the workbench and in each member.
pub const MOUNT: &str = "/run/paddock/workbench";
/// The variable that names the socket folder inside a sandbox. srt cannot
/// bind-mount, so there it is the host folder.
pub const ENV: &str = "PADDOCK_WORKBENCH";
/// The file name of the tmux socket in the socket folder.
pub const SOCKET: &str = "tmux";
/// The name of the one tmux session.
pub const SESSION: &str = "workbench";
/// How long a start waits for the tmux socket.
const START_WAIT: Duration = Duration::from_secs(45);
/// How often the workbench checks whether a member still runs.
const CHECK_EVERY: Duration = Duration::from_secs(2);

/// The script of the workbench sandbox: start the tmux server, wait while
/// the session lives, and end the server on SIGTERM. tmux leaves its
/// socket file when the server ends, so the script removes it.
const SCRIPT: &str = r#"S="${PADDOCK_WORKBENCH:-/run/paddock/workbench}/tmux"
if ! command -v tmux >/dev/null 2>&1; then
  echo "paddock workbench: tmux is not in the sandbox" >&2
  exit 127
fi
trap 'tmux -S "$S" kill-server 2>/dev/null; rm -f "$S"; exit 0' TERM INT HUP
tmux -S "$S" new-session -d -s workbench -c "$PWD" || exit 1
while tmux -S "$S" has-session -t workbench 2>/dev/null; do sleep 1; done
rm -f "$S"
"#;

/// The built-in profile of the workbench: a shell, no login, no MCP.
pub fn profile() -> crate::profile::Profile {
    let mut p: crate::profile::Profile = toml::from_str(
        "name = \"workbench\"\ndescription = \"The shared terminal of a pasture\"\n[locate]\ncommands = [\"sh\"]\n",
    )
    .expect("the workbench profile is valid");
    p.launch.args = vec!["-c".into(), SCRIPT.into()];
    p
}

/// The socket folder of a pasture on the host.
pub fn dir(env: &HostEnv, project: &Path, name: &str) -> PathBuf {
    super::folder(env, project, name).join("workbench")
}

/// The tmux socket of a pasture on the host.
pub fn socket(env: &HostEnv, project: &Path, name: &str) -> PathBuf {
    dir(env, project, name).join(SOCKET)
}

/// The log of the workbench sandbox, outside the folder that members see.
pub fn log_file(env: &HostEnv, project: &Path, name: &str) -> PathBuf {
    super::folder(env, project, name).join("workbench.log")
}

/// The tmux socket inside a sandbox, from its `PADDOCK_WORKBENCH`.
pub fn socket_in_sandbox(var: Option<&str>) -> PathBuf {
    Path::new(var.filter(|v| !v.is_empty()).unwrap_or(MOUNT)).join(SOCKET)
}

/// The command that attaches a terminal to the workbench.
pub fn attach_command(socket: &Path) -> String {
    format!("tmux -S {} attach -t {SESSION}", socket.display())
}

/// Whether `[pastures.NAME] workbench = true`.
pub fn allowed(env: &HostEnv, name: &str) -> bool {
    super::load(env)
        .ok()
        .and_then(|all| all.get(name).map(|p| p.workbench))
        .unwrap_or(false)
}

/// The running workbench of a pasture, if any.
pub fn running<'a>(list: &'a [Running], project: &Path, name: &str) -> Option<&'a Running> {
    crate::state::pasture_members(list, project, name)
        .into_iter()
        .find(|r| r.harness == NAME)
}

/// The running members of a pasture, without the workbench.
pub fn members<'a>(list: &'a [Running], project: &Path, name: &str) -> Vec<&'a Running> {
    crate::state::pasture_members(list, project, name)
        .into_iter()
        .filter(|r| r.harness != NAME)
        .collect()
}

/// Whether a running workbench stops now. `seen`: a member ran at some
/// time since the workbench started; `members`: how many run now. A
/// workbench that the user started before any member waits for them.
pub fn should_stop(allowed: bool, seen: bool, members: usize) -> bool {
    !allowed || (seen && members == 0)
}

/// One line in the audit log: start, stop or join, and who.
pub fn audit(env: &HostEnv, project: &Path, name: &str, event: &str, who: &str) {
    let line = serde_json::json!({
        "time": crate::state::now(),
        "op": "workbench",
        "scope": "pasture",
        "project": project,
        "pasture": name,
        "event": event,
        "by": who,
    });
    let f = crate::kv::audit_file(env);
    match crate::privfs::append(&f) {
        Ok(mut file) => {
            let _ = writeln!(file, "{line}");
        }
        Err(e) => log::warn!("cannot open the kv audit log {}: {e}", f.display()),
    }
}

/// Whether a tmux server answers on `sock`.
fn answers(sock: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(sock).is_ok()
}

/// What `start` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Started {
    /// It ran already.
    Already,
    /// This call started it.
    Now,
}

/// Start the workbench of a pasture, or find it running. `who` goes into
/// the audit log. Waits until its tmux socket answers.
pub fn start(env: &HostEnv, project: &Path, name: &str, who: &str) -> Result<Started> {
    super::check_name(name)?;
    if !allowed(env, name) {
        bail!(
            "the pasture '{name}' has no workbench. Allow it in {}:\n\n[pastures.{name}]\nworkbench = true",
            env.paddock_config().join("config.toml").display()
        );
    }
    let d = dir(env, project, name);
    crate::privfs::dir(&d)?;
    // One start at a time: a second caller waits here, then finds it.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(super::folder(env, project, name).join("workbench.lock"))
        .context("cannot open the workbench lock")?;
    use std::os::fd::AsRawFd as _;
    // SAFETY: flock on an open file descriptor.
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    let sock = d.join(SOCKET);
    if running(&Running::list(env), project, name).is_some() {
        if wait_for(&sock, START_WAIT, || true) {
            return Ok(Started::Already);
        }
        bail!("the workbench of '{name}' runs, but its tmux socket does not answer");
    }
    // A socket that a killed server left.
    let _ = std::fs::remove_file(&sock);
    let log = crate::privfs::append(&log_file(env, project, name))?;
    let exe = std::env::current_exe().context("cannot find the paddock executable")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["pasture", "workbench", name, "--serve", "--project"])
        .arg(project)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    use std::os::unix::process::CommandExt as _;
    // SAFETY: setsid is async-signal-safe. Its own session, so that the
    // terminal of the caller can close.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("cannot start the workbench")?;
    let mut ended = None;
    let up = wait_for(&sock, START_WAIT, || match child.try_wait() {
        Ok(Some(s)) => {
            ended = Some(s);
            false
        }
        _ => true,
    });
    if !up {
        if ended.is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        bail!(
            "the workbench of '{name}' did not start. Its log: {}",
            log_file(env, project, name).display()
        );
    }
    // Reap it when it ends; it outlives this process otherwise.
    std::thread::spawn(move || child.wait());
    audit(env, project, name, "start", who);
    Ok(Started::Now)
}

/// Wait until `sock` answers. `go` returns false to give up early.
fn wait_for(sock: &Path, max: Duration, mut go: impl FnMut() -> bool) -> bool {
    let t = Instant::now();
    while t.elapsed() < max {
        if answers(sock) {
            return true;
        }
        if !go() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Stop the workbench of a pasture. Returns false when none ran.
pub fn stop(env: &HostEnv, project: &Path, name: &str) -> Result<bool> {
    let list = Running::list(env);
    let Some(r) = running(&list, project, name) else {
        return Ok(false);
    };
    signal_stop(env, r);
    let sock = socket(env, project, name);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(15) {
        if !crate::state::alive(r.pid) && !sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(true)
}

fn signal_stop(env: &HostEnv, r: &Running) {
    // A frozen process gets SIGTERM only when it thaws.
    let _ = crate::freeze::thaw(env, &r.id, true);
    // SAFETY: kill with a pid from the run record.
    unsafe { libc::kill(r.pid as i32, libc::SIGTERM) };
}

/// The workbench as `pasture list` and `pasture members` show it.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq, Default)]
pub struct State {
    /// `workbench = true` in the user configuration.
    pub allowed: bool,
    pub running: bool,
    /// Its sandbox id, while it runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
}

pub fn state(env: &HostEnv, project: &Path, name: &str) -> State {
    let list = Running::list(env);
    let r = running(&list, project, name);
    State {
        allowed: allowed(env, name),
        running: r.is_some(),
        sandbox: r.map(|r| r.id.clone()),
    }
}

/// `paddock pasture workbench NAME --serve`: build and run the workbench
/// sandbox. It runs detached; `start` starts it.
pub fn serve(env: &HostEnv, project: &Path, name: &str) -> Result<i32> {
    super::check_name(name)?;
    let factory = crate::profile::HarnessFactory::new(env)?;
    let harness = factory.adopt(profile(), "built-in workbench")?;
    let opts = crate::build::Options {
        harness: NAME.into(),
        project: Some(project.to_path_buf()),
        network: Some(crate::spec::Network::None),
        inherit_env: Some(false),
        auth: Some("shared".into()),
        pasture: Some(name.into()),
        workbench: true,
        ..Default::default()
    };
    let mut built = crate::build::build_with(env, &opts, harness)?;
    neutral(&mut built.spec);
    // An empty home at each start.
    empty_home(&built.spec.home_src)?;
    let backend = crate::run::select_backend(env, &built)?;
    let id = built.spec.id.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = {
        let (env, project, name, stop) = (
            env.clone(),
            project.to_path_buf(),
            name.to_string(),
            stop.clone(),
        );
        std::thread::spawn(move || watch(&env, &project, &name, &id, &stop))
    };
    let out = crate::run::run(env, built, backend.as_ref(), false, true);
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = watcher.join();
    // A server that was killed leaves its socket.
    let _ = std::fs::remove_file(socket(env, project, name));
    audit(env, project, name, "end", "paddock");
    Ok(out?.code)
}

/// Stop the workbench when the user turns it off or the last member ends.
fn watch(
    env: &HostEnv,
    project: &Path,
    name: &str,
    id: &str,
    stop: &std::sync::atomic::AtomicBool,
) {
    let mut seen = false;
    let since = crate::state::now();
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(CHECK_EVERY);
        let list = Running::list(env);
        let Some(me) = list.iter().find(|r| r.id == id) else {
            continue;
        };
        let n = members(&list, project, name).len();
        // A member that came and went between two checks counts too.
        seen = seen || n > 0 || started_since(env, project, name, since);
        let on = allowed(env, name);
        if should_stop(on, seen, n) {
            let why = if on {
                "paddock: the last member ended"
            } else {
                "paddock: workbench = false"
            };
            log::info!("the workbench of '{name}' stops ({why})");
            audit(env, project, name, "stop", why);
            signal_stop(env, me);
            return;
        }
    }
}

/// Whether a member of the pasture started at `since` or later, from the
/// registry of paddock kv.
fn started_since(env: &HostEnv, project: &Path, name: &str, since: u64) -> bool {
    let reg = crate::kv::shard::registry(&crate::kv::kv_root(env));
    if !reg.is_file() {
        return false;
    }
    crate::kv::store::Store::open(&reg)
        .and_then(|st| st.sandboxes_of(project))
        .map(|rs| {
            rs.iter().any(|r| {
                r.pasture.as_deref() == Some(name) && r.harness != NAME && r.started >= since
            })
        })
        .unwrap_or(false)
}

/// `paddock-host pasture workbench` of a member, on the host side: start
/// the workbench when it does not run, and log the join.
pub fn join(env: &HostEnv, id: &crate::kv::Identity) -> crate::kv::Response {
    use crate::kv::{Response, Status};
    let Some(name) = id.pasture.as_deref().filter(|_| !id.is_child()) else {
        return Response::new(
            Status::Disabled,
            "this sandbox is in no pasture: the user starts members with --pasture NAME",
        );
    };
    if id.harness == NAME {
        return Response::new(Status::Invalid, "this sandbox is the workbench");
    }
    if !allowed(env, name) {
        return Response::new(
            Status::Disabled,
            format!("the pasture '{name}' has no workbench: the user allows it with workbench = true in [pastures.{name}]"),
        );
    }
    let who = format!("{} (sandbox {})", id.harness, id.sandbox);
    match start(env, &id.project, name, &who) {
        Ok(s) => {
            audit(env, &id.project, name, "join", &who);
            Response::done(serde_json::json!({
                "started": s == Started::Now,
                "session": SESSION,
            }))
        }
        Err(e) => Response::new(Status::Failed, format!("{e:#}")),
    }
}

/// Keep only what the workbench needs: no extra mounts, setup lines or
/// host ports from the project settings.
fn neutral(spec: &mut crate::spec::SandboxSpec) {
    spec.mounts.retain(|m| !m.origin.starts_with("manifest "));
    spec.host_ports.clear();
    spec.publish.clear();
    spec.lan = false;
}

/// Remove what an earlier run left in the workbench home.
fn empty_home(home: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(home) else {
        return Ok(());
    };
    for e in entries.flatten() {
        // paddock's own record of whose home it is.
        if e.file_name().to_string_lossy().starts_with(".paddock") {
            continue;
        }
        let p = e.path();
        let r = if e.file_type().is_ok_and(|t| t.is_dir()) {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
        r.with_context(|| format!("cannot empty the workbench home: {}", p.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(t: &Path) -> HostEnv {
        HostEnv::from_parts(t.join("home"), "t".into(), 1000, 1000, Default::default())
    }

    #[test]
    fn socket_paths() {
        let t = tempfile::tempdir().unwrap();
        let env = env(t.path());
        let p = Path::new("/src/app");
        let s = socket(&env, p, "team");
        assert_eq!(
            s,
            super::super::folder(&env, p, "team").join("workbench/tmux")
        );
        // Members never see the log or the lock.
        assert!(!log_file(&env, p, "team").starts_with(dir(&env, p, "team")));
        assert_eq!(
            socket_in_sandbox(None),
            Path::new("/run/paddock/workbench/tmux")
        );
        assert_eq!(socket_in_sandbox(Some("")), Path::new(MOUNT).join("tmux"));
        assert_eq!(socket_in_sandbox(Some("/h/wb")), Path::new("/h/wb/tmux"));
        assert_eq!(
            attach_command(Path::new("/run/paddock/workbench/tmux")),
            "tmux -S /run/paddock/workbench/tmux attach -t workbench"
        );
    }

    #[test]
    fn start_and_stop_rules() {
        // Started by the user before any member: it waits.
        assert!(!should_stop(true, false, 0));
        assert!(!should_stop(true, true, 2));
        // The last member ended.
        assert!(should_stop(true, true, 0));
        // Turned off in the configuration.
        assert!(should_stop(false, false, 0));
        assert!(should_stop(false, true, 3));
    }

    #[test]
    fn start_needs_the_configuration() {
        let t = tempfile::tempdir().unwrap();
        let env = env(t.path());
        std::fs::create_dir_all(env.paddock_config()).unwrap();
        let e = start(&env, Path::new("/src/app"), "team", "user").unwrap_err();
        assert!(format!("{e:#}").contains("workbench = true"), "{e:#}");
        assert!(!allowed(&env, "team"));
        std::fs::write(
            env.paddock_config().join("config.toml"),
            "[pastures.team]\nmembers = [\"sh\"]\nworkbench = true\n",
        )
        .unwrap();
        assert!(allowed(&env, "team"));
        let s = state(&env, Path::new("/src/app"), "team");
        assert!(s.allowed && !s.running);
        // Nothing to stop.
        assert!(!stop(&env, Path::new("/src/app"), "team").unwrap());
    }

    #[test]
    fn running_and_members_tell_the_workbench_apart() {
        let r = |id: &str, harness: &str| -> Running {
            serde_json::from_value(serde_json::json!({
                "id": id, "harness": harness, "project": "/src/app", "backend": "bwrap",
                "network": "none", "gui": "off", "pid": 1, "started": 1,
                "kv_project": "/src/app", "pasture": "team",
            }))
            .unwrap()
        };
        let list = vec![r("a1", "claude"), r("w1", NAME), r("b1", "codex")];
        let p = Path::new("/src/app");
        assert_eq!(running(&list, p, "team").unwrap().id, "w1");
        let m: Vec<_> = members(&list, p, "team")
            .iter()
            .map(|r| r.id.clone())
            .collect();
        assert_eq!(m, ["a1", "b1"]);
        assert!(running(&list, p, "other").is_none());
    }

    #[test]
    fn the_home_is_emptied() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path();
        std::fs::create_dir_all(h.join(".config/x")).unwrap();
        std::fs::write(h.join(".bash_history"), "ls").unwrap();
        std::fs::write(h.join(".paddock-origin.json"), "{}").unwrap();
        empty_home(h).unwrap();
        let left: Vec<_> = std::fs::read_dir(h)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, [".paddock-origin.json"]);
    }

    #[test]
    fn the_profile_has_no_login() {
        let p = profile();
        assert!(p.state.auth.is_empty() && p.mcp.is_empty() && p.oneshot.is_none());
        assert!(p.launch.args[1].contains("new-session -d -s workbench"));
    }
}
