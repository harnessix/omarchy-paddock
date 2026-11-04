//! Broker tests with a fixed answer instead of a person.

use super::*;
use std::collections::BTreeMap;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

mod approval;
mod borrow;
#[path = "forge.rs"]
mod forge_run;
#[path = "git.rs"]
mod push;
mod tools;

pub(in crate::broker::tests) struct Fixed {
    answer: Option<Answer>,
    asked: AtomicUsize,
}

impl Approver for Fixed {
    fn ask(&self, _: &HostEnv, _: &Pending, _: Duration) -> Option<Answer> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.answer
    }
}

pub(in crate::broker::tests) fn setup(
    answer: Option<Answer>,
    policy: HostPolicy,
) -> (tempfile::TempDir, Broker, Arc<Fixed>) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let project = tmp.path().join("work/app");
    std::fs::create_dir_all(&project).unwrap();
    // The child runs /bin/pwd. That is getcwd, the canonical path.
    // On macOS /var is /private/var. On Linux this is the same path.
    let project = std::fs::canonicalize(&project).unwrap();
    let mut vars = BTreeMap::new();
    vars.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    let env = HostEnv::from_parts(home, "tester".into(), 1000, 1000, vars);
    let fixed = Arc::new(Fixed {
        answer,
        asked: AtomicUsize::new(0),
    });
    let ctx = Context {
        id: "t-1".into(),
        harness: "claude".into(),
        project,
        repo: None,
        chain: vec![],
        depth: 0,
        role: None,
    };
    let mut b = Broker::new(env.clone(), ctx, policy, fixed.clone());
    // The executor runs in this process: the same signature checks, no
    // isolation (the tests of execbox and tests/gateway.rs cover that).
    b.dispatch = Some(Arc::new(crate::gatewayd::InProcess {
        key: vec![9u8; 32],
        seen: Default::default(),
        env,
        isolation: crate::execbox::Isolation::None,
    }));
    (tmp, b, fixed)
}

pub(in crate::broker::tests) fn req(cap: &str, args: serde_json::Value) -> Request {
    let args = args
        .as_object()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();
    Request {
        v: PROTOCOL,
        cap: cap.into(),
        args,
        reason: "test".into(),
    }
}

pub(in crate::broker::tests) fn exec(argv: &[&str]) -> Request {
    req("exec", serde_json::json!({ "argv": argv }))
}

/// Swaps the fetch source for a link to a credential file while the user
/// decides, as a sandbox could.
pub(in crate::broker::tests) struct Swapper {
    from: PathBuf,
    to: PathBuf,
    seen: Mutex<Option<String>>,
}

impl Approver for Swapper {
    fn ask(&self, _: &HostEnv, p: &Pending, _: Duration) -> Option<Answer> {
        *self.seen.lock().unwrap() = Some(p.detail.clone());
        std::fs::remove_file(&self.from).unwrap();
        std::os::unix::fs::symlink(&self.to, &self.from).unwrap();
        Some(Answer::Once)
    }
}

pub(in crate::broker::tests) fn audit_lines(b: &Broker) -> Vec<serde_json::Value> {
    std::fs::read_to_string(audit_file(&b.env))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

const SECRET: &str = "hunter2-correct-horse-battery";

pub(in crate::broker::tests) fn pending_for(b: &Broker, detail: &str) -> Pending {
    Pending {
        id: "r-1-abcdef12".into(),
        sandbox: b.ctx.id.clone(),
        harness: "claude".into(),
        project: b.ctx.project.clone(),
        capability: "clipboard-write".into(),
        summary: "set the host clipboard".into(),
        detail: detail.into(),
        reason: String::new(),
        created: state::now(),
        expires: state::now() + 1,
        session_allowed: true,
        choices: Vec::new(),
        started_by: vec![],
    }
}

/// A fake `notify-send` that records its arguments. paddock must not run
/// it: arguments of a host process are visible to every local user.
pub(in crate::broker::tests) fn fake_notify_send(b: &mut Broker, dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let record = dir.join("argv.txt");
    let script = bin.join("notify-send");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n",
            record.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut vars = BTreeMap::new();
    vars.insert(
        "PATH".into(),
        format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    b.env = HostEnv::from_parts(b.env.home.clone(), "tester".into(), 1000, 1000, vars);
    record
}

pub(in crate::broker::tests) fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@localhost"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A broker whose project is a git repository with a bare `origin`.
pub(in crate::broker::tests) fn setup_repo(
    answer: Option<Answer>,
    policy: HostPolicy,
) -> (tempfile::TempDir, Broker, Arc<Fixed>, String) {
    let (tmp, mut b, fixed) = setup(answer, policy);
    let remote = tmp.path().join("remote.git");
    git_in(
        tmp.path(),
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );
    let p = b.ctx.project.clone();
    git_in(&p, &["init", "-q", "-b", "main"]);
    git_in(&p, &["commit", "-q", "--allow-empty", "-m", "one"]);
    git_in(&p, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git_in(
        &p,
        &[
            "config",
            "remote.upstream.url",
            "https://github.com/demo/app.git",
        ],
    );
    b.ctx.repo = git::Repo::snapshot(&p);
    let sha = git_in(&p, &["rev-parse", "HEAD"]);
    (tmp, b, fixed, sha)
}

pub(in crate::broker::tests) fn push_req(
    remote: &str,
    sha: &str,
    dst: &str,
    force: bool,
) -> Request {
    req(
        "git-push",
        serde_json::json!({ "remote": remote, "url": remote,
            "refs": [{ "src": "HEAD", "sha": sha, "dst": dst, "force": force }] }),
    )
}

/// A fixed answer with a note, as the user gives it on the card.
pub(in crate::broker::tests) struct Noted {
    answer: Answer,
    note: Option<String>,
    asked: AtomicUsize,
}

impl Approver for Noted {
    fn ask(&self, _: &HostEnv, _: &Pending, _: Duration) -> Option<Answer> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Some(self.answer)
    }
    fn ask_verdict(&self, env: &HostEnv, p: &Pending, wait: Duration) -> Option<Verdict> {
        self.ask(env, p, wait).map(|answer| Verdict {
            answer,
            note: self.note.clone(),
            who: "user",
        })
    }
}

pub(in crate::broker::tests) fn noted(
    answer: Answer,
    note: &str,
) -> (tempfile::TempDir, Broker, Arc<Noted>) {
    let (t, mut b, _) = setup(None, HostPolicy::default());
    let n = Arc::new(Noted {
        answer,
        note: Some(note.into()),
        asked: AtomicUsize::new(0),
    });
    b.approver = n.clone();
    (t, b, n)
}

/// A broker whose user configuration holds `config`, with a fake `faketool`
/// on the executor's PATH. The tool prints its arguments, its folder and its
/// environment.
pub(in crate::broker::tests) fn tool_setup(
    answer: Option<Answer>,
    config: &str,
) -> (tempfile::TempDir, Broker, Arc<Fixed>) {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let project = tmp.path().join("work/app");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("faketool");
    std::fs::write(
        &script,
        "#!/bin/sh\necho \"args:$*\"\necho \"pwd:$(pwd)\"\nenv | sort\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut vars = BTreeMap::new();
    vars.insert("PATH".into(), format!("{}:/usr/bin:/bin", bin.display()));
    let env = HostEnv::from_parts(home, "tester".into(), 1000, 1000, vars);
    std::fs::create_dir_all(env.paddock_config()).unwrap();
    std::fs::write(env.paddock_config().join("config.toml"), config).unwrap();
    std::fs::create_dir_all(&env.home).unwrap();
    let policy = HostPolicy::load(&env).unwrap();
    let fixed = Arc::new(Fixed {
        answer,
        asked: AtomicUsize::new(0),
    });
    let ctx = Context {
        id: "t-1".into(),
        harness: "claude".into(),
        project,
        repo: None,
        chain: vec![],
        depth: 0,
        role: None,
    };
    let mut b = Broker::new(env.clone(), ctx, policy, fixed.clone());
    b.dispatch = Some(Arc::new(crate::gatewayd::InProcess {
        key: vec![9u8; 32],
        seen: Default::default(),
        env,
        isolation: crate::execbox::Isolation::None,
    }));
    (tmp, b, fixed)
}

pub(in crate::broker::tests) fn tool(name: &str, args: &[&str]) -> Request {
    req(
        "tool",
        serde_json::json!({
            "name": name,
            "args": args,
            // A sandbox cannot hand the tool variables: this is ignored.
            "env": { "SANDBOX_SECRET": SECRET },
        }),
    )
}

const TOOLS: &str = "[host.tools.compose]\ncommand = [\"faketool\", \"compose\"]\nargs = [\"up -d\", \"logs *\"]\nmode = \"allow\"\n\
[host.tools.ask]\ncommand = [\"faketool\"]\nargs = [\"run **\"]\n\
[host.tools.home]\ncommand = [\"faketool\"]\ncwd = \"home\"\nmode = \"allow\"\n\
[host.tools.off]\ncommand = [\"faketool\"]\nmode = \"deny\"\n";

/// A broker with fake `tea` and `glab` on the host's PATH, and a project
/// with remotes on codeberg.org and a self-hosted GitLab. The fakes print
/// their arguments, one per line, and their environment.
pub(in crate::broker::tests) fn forge_setup(
    answer: Option<Answer>,
) -> (tempfile::TempDir, Broker, Arc<Fixed>) {
    use std::os::unix::fs::PermissionsExt;
    let (tmp, mut b, fixed, _) = setup_repo(answer, HostPolicy::default());
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for tool in ["tea", "glab"] {
        let script = bin.join(tool);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"tool:{tool}\"\nfor a in \"$@\"; do echo \"arg:$a\"; done\necho \"stdin:$(cat)\"\nenv | sort\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    b.env.path = vec![bin, "/usr/bin".into(), "/bin".into()];
    let tea_cfg = b.env.config_dir.join("tea");
    std::fs::create_dir_all(&tea_cfg).unwrap();
    std::fs::write(
        tea_cfg.join("config.yml"),
        "logins:\n- name: cb\n  url: https://codeberg.org\n  token: host-token\n  default: true\n",
    )
    .unwrap();
    let p = b.ctx.project.clone();
    git_in(
        &p,
        &[
            "remote",
            "add",
            "forgejo",
            "https://codeberg.org/demo/tool.git",
        ],
    );
    git_in(
        &p,
        &[
            "remote",
            "add",
            "lab",
            "git@gitlab.example.org:team/apps/web.git",
        ],
    );
    b.ctx.repo = git::Repo::snapshot(&p);
    (tmp, b, fixed)
}

pub(in crate::broker::tests) fn forge_req(tool: &str, argv: &[&str]) -> Request {
    req(
        "forge",
        serde_json::json!({
            "tool": tool,
            "argv": argv,
            "branch": "fix",
            // A sandbox cannot hand the tool variables: this is ignored.
            "env": { "GITEA_TOKEN": SECRET, "GITLAB_TOKEN": SECRET },
        }),
    )
}

/// The arguments and the environment that a fake forge tool printed.
pub(in crate::broker::tests) fn fake_output(r: &Response) -> (Vec<String>, String) {
    let out = r.result["stdout"].as_str().unwrap_or("").to_string();
    assert!(!out.contains(SECRET), "{out}");
    for line in out
        .lines()
        .filter(|l| l.contains('=') && !l.starts_with("arg:"))
    {
        let k = line.split('=').next().unwrap();
        assert!(
            !k.contains("TOKEN"),
            "a token variable reached the tool: {line}"
        );
    }
    let args = out
        .lines()
        .filter_map(|l| l.strip_prefix("arg:"))
        .map(String::from)
        .collect();
    (args, out)
}
