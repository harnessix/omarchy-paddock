//! Tests for the spawn rules: limits, inheritance, approvals and privacy.

use super::*;
use crate::broker::{
    Answer, Approver, Broker, Context, HostPolicy, Pending, Request, Status as RStatus, PROTOCOL,
};
use crate::build::{build_with, Options};
use crate::profile::{Harness, Profile};
use crate::spec::Mount;
use std::sync::atomic::{AtomicUsize, Ordering};

const SECRET_TASK: &str = "TASK-TEXT-7f3a: refactor the parser";

fn env_in(tmp: &Path) -> HostEnv {
    let mut vars = BTreeMap::new();
    vars.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    vars.insert("PLAIN_SETTING".into(), "1".into());
    vars.insert(
        "XDG_RUNTIME_DIR".into(),
        tmp.join("run").display().to_string(),
    );
    std::fs::create_dir_all(tmp.join("run")).unwrap();
    HostEnv::from_parts(tmp.join("home"), "tester".into(), 1000, 1000, vars)
}

fn write_user_config(env: &HostEnv, text: &str) {
    let dir = env.paddock_config();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), text).unwrap();
}

fn fake_harness(oneshot: &str) -> Harness {
    let profile: Profile = toml::from_str(&format!("name = \"fake\"\n{oneshot}")).unwrap();
    Harness {
        profile,
        exe: PathBuf::from("/bin/sh"),
        install_mounts: vec![],
        path_dirs: vec![],
        source: "test".into(),
    }
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@localhost"])
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn parent_built(env: &HostEnv, project: &Path) -> Built {
    let opts = Options {
        harness: "fake".into(),
        project: Some(project.to_path_buf()),
        network: Some(Network::None),
        inherit_env: Some(false),
        assume_yes: true,
        ..Default::default()
    };
    build_with(env, &opts, fake_harness("")).unwrap()
}

fn modes(pairs: &[(&str, Mode)]) -> BTreeMap<String, Mode> {
    pairs.iter().map(|(k, m)| (k.to_string(), *m)).collect()
}

#[test]
fn config_defaults_allow_no_harness() {
    let c = SpawnCfg::default();
    assert!(c.harnesses.is_empty());
    assert_eq!((c.max_running, c.max_depth), (2, 1));
    assert!(!c.allows("codex"));
    // Depth 0 is a sandbox that the user started; its child is depth 1.
    assert!(c.depth_allows(0));
    assert!(!c.depth_allows(1));
    let none = SpawnCfg {
        max_depth: 0,
        ..c.clone()
    };
    assert!(!none.depth_allows(0));
}

#[test]
fn config_is_checked() {
    let doc = |t: &str| SpawnCfg::from_doc(&toml::from_str(t).unwrap());
    let c =
        doc("[spawn]\nharnesses = [\"codex\"]\nmax_running = 1\nmax_depth = 2\ntimeout = 600\n")
            .unwrap();
    assert!(c.allows("codex") && !c.allows("claude"));
    assert_eq!((c.max_running, c.max_depth, c.timeout), (1, 2, 600));
    for bad in [
        "[spawn]\nmax_running = 0\n",
        "[spawn]\nmax_running = 99\n",
        "[spawn]\nmax_depth = 9\n",
        "[spawn]\ntimeout = 5\n",
        "[spawn]\nharnesses = [\"../x\"]\n",
        "[spawn]\nharnesses = \"codex\"\n",
        "[spawn]\nnested = true\n",
    ] {
        assert!(doc(bad).is_err(), "{bad}");
    }
}

#[test]
fn a_project_file_cannot_enable_spawn() {
    let tmp = tempfile::tempdir().unwrap();
    let env = env_in(tmp.path());
    let project = tmp.path().join("app");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("paddock.toml"),
        "[host]\nspawn = \"allow\"\n[spawn]\nharnesses = [\"codex\"]\nmax_depth = 4\n",
    )
    .unwrap();
    // The project file loads (its [spawn] table is ignored with a warning)...
    let loaded = Manifest::load(&env, &project).unwrap();
    assert!(loaded.merged.spawn.is_none());
    // ...and neither the mode nor the limits change.
    assert_eq!(SpawnCfg::load(&env).unwrap(), SpawnCfg::default());
    assert_eq!(HostPolicy::load(&env).unwrap().mode("spawn"), Mode::Ask);
    // The user configuration is the only source.
    write_user_config(
        &env,
        "[host]\nspawn = \"deny\"\n[spawn]\nharnesses = [\"codex\"]\n",
    );
    assert!(SpawnCfg::load(&env).unwrap().allows("codex"));
    assert_eq!(HostPolicy::load(&env).unwrap().mode("spawn"), Mode::Deny);
}

#[test]
fn child_modes_are_never_wider_than_the_parent() {
    let now = modes(&[
        ("exec", Mode::Ask),
        ("git-push", Mode::Allow),
        ("notify", Mode::Allow),
        ("spawn", Mode::Allow),
        ("fetch", Mode::Ask),
    ]);
    let parent = modes(&[
        ("exec", Mode::Ask),
        ("git-push", Mode::Ask),
        ("notify", Mode::Allow),
        ("spawn", Mode::Deny),
    ]);
    let c = child_modes(&now, &parent);
    assert_eq!(c["exec"], Mode::Ask);
    assert_eq!(c["git-push"], Mode::Ask);
    assert_eq!(c["notify"], Mode::Allow);
    assert_eq!(c["spawn"], Mode::Deny);
    // A capability the parent did not know is denied.
    assert_eq!(c["fetch"], Mode::Deny);
    // The configuration may have become stricter since the parent started.
    let stricter = modes(&[("notify", Mode::Deny)]);
    assert_eq!(child_modes(&stricter, &parent)["notify"], Mode::Deny);
}

#[test]
fn a_child_spec_wider_than_the_parent_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let env = env_in(tmp.path());
    let project = tmp.path().join("app");
    let shared = tmp.path().join("shared");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&shared).unwrap();
    let parent = parent_built(&env, &project);
    let c = Ceiling::of(&parent, "bwrap", &modes(&[("exec", Mode::Ask)]));
    assert_eq!(c.chain, vec![parent.spec.id.clone()]);
    assert_eq!(c.depth, 0);
    assert!(!c.inherit_env);
    c.check(&parent, "bwrap").unwrap();

    let refused = |f: &dyn Fn(&mut Built), what: &str| {
        let mut b = parent_built(&env, &project);
        f(&mut b);
        let e = c.check(&b, "bwrap").unwrap_err().to_string();
        assert!(e.contains("more than its parent"), "{what}: {e}");
    };
    refused(&|b| b.spec.network = Network::Egress, "network");
    refused(&|b| b.spec.network = Network::Host, "host network");
    refused(&|b| b.spec.lan = true, "lan");
    refused(&|b| b.inherit_env = true, "environment");
    refused(&|b| b.spec.gui.mode = GuiMode::Headless, "display");
    refused(&|b| b.spec.gui.gpu = true, "gpu");
    refused(&|b| b.spec.publish.push(8080), "publish");
    refused(
        &|b| b.home_key = PathBuf::from("/home/demo/other"),
        "project",
    );
    refused(
        &|b| b.spec.set_env("PLAIN_SETTING", "1", "host env (inherited)"),
        "inherited variable",
    );
    refused(
        &|b| b.spec.set_env("EXTRA", "1", "manifest env.set"),
        "manifest variable",
    );
    refused(
        &|b| {
            b.spec
                .add_mount(Mount::same(&shared, false, "manifest mounts.ro"))
        },
        "mount",
    );
    refused(
        &|b| b.spec.add_host_port(5432, "manifest network.host_ports"),
        "host port",
    );
    // Another backend is refused too.
    let b = parent_built(&env, &project);
    assert!(c.check(&b, "docker").is_err());

    // A read-only mount of the parent cannot become writable in the child.
    let mut p = parent_built(&env, &project);
    p.spec
        .add_mount(Mount::same(&shared, false, "manifest mounts.ro"));
    let c2 = Ceiling::of(&p, "bwrap", &BTreeMap::new());
    c2.check(&p, "bwrap").unwrap();
    let mut b = parent_built(&env, &project);
    b.spec
        .add_mount(Mount::same(&shared, true, "manifest mounts.rw"));
    assert!(c2.check(&b, "bwrap").is_err());
}

#[test]
fn the_child_settings_have_no_display_and_no_published_ports() {
    let tmp = tempfile::tempdir().unwrap();
    let env = env_in(tmp.path());
    let project = tmp.path().join("app");
    std::fs::create_dir_all(&project).unwrap();
    let mut p = parent_built(&env, &project);
    p.manifest.gui.mode = Some(GuiMode::Host);
    p.manifest.gui.gpu = Some(true);
    p.manifest.network.publish = vec![3000];
    let c = Ceiling::of(&p, "bwrap", &BTreeMap::new());
    assert_eq!(c.manifest.gui.mode, None);
    assert_eq!(c.manifest.gui.gpu, None);
    assert!(c.manifest.network.publish.is_empty());
}

#[test]
fn a_child_build_uses_the_snapshot_and_keeps_the_prompt_out_of_argv() {
    let tmp = tempfile::tempdir().unwrap();
    let mut vars = BTreeMap::new();
    vars.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    vars.insert(
        "XDG_DATA_HOME".into(),
        tmp.path().join("data").display().to_string(),
    );
    let env = HostEnv::from_parts(tmp.path().join("home"), "t".into(), 1000, 1000, vars);
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
    let repo = std::fs::canonicalize(&repo).unwrap();
    // The parent can edit the project file. The child never reads it.
    std::fs::write(repo.join("paddock.toml"), "[network]\nmode = \"host\"\n").unwrap();
    for (oneshot, file) in [
        ("[oneshot]\nargs = [\"-c\", \"cat\"]\nprompt = \"stdin\"\n", false),
        (
            "[oneshot]\nargs = [\"-c\", \"cat \\\"$0\\\"\", \"{prompt_file}\"]\nprompt = \"file\"\n",
            true,
        ),
    ] {
        let plan = ChildPlan {
            id: sandbox_id_for("fake", if file { "0011aabc" } else { "0011aabb" }),
            manifest: Manifest::default(),
            branch: branch_for("fake", if file { "0011aabc" } else { "0011aabb" }),
            base_from: repo.clone(),
        };
        let opts = Options {
            harness: "fake".into(),
            project: Some(repo.clone()),
            network: Some(Network::None),
            gui: Some(GuiMode::Off),
            worktree: true,
            child: Some(plan.clone()),
            ..Default::default()
        };
        let privacy = "[privacy]\nenv = { FAKE_TELEMETRY = \"0\" }\nargs = [\"--no-telemetry\"]\n";
        let b = build_with(&env, &opts, fake_harness(&format!("{privacy}{oneshot}"))).unwrap();
        assert_eq!(b.spec.network, Network::None, "the project file was read");
        // The privacy defaults apply to a child too: its variables, and its
        // flags before the one-shot arguments.
        assert!(b
            .spec
            .env
            .get("FAKE_TELEMETRY")
            .is_some_and(|v| v.origin.contains("privacy")));
        assert_eq!(b.spec.command[1], "--no-telemetry");
        let w = b.worktree.as_ref().unwrap();
        assert_eq!(w.branch, plan.branch);
        assert_eq!(b.home_key, repo);
        assert!(b.spec.command.iter().all(|a| !a.contains(SECRET_TASK)));
        let named = b.spec.command.iter().any(|a| a.ends_with(PROMPT_IN_HOME));
        assert_eq!(named, file, "{:?}", b.spec.command);
        if file {
            assert!(b
                .spec
                .command
                .iter()
                .any(|a| a.starts_with(&b.spec.home.display().to_string())));
        }
    }
}

/// Records every question; answers with a fixed answer.
struct Recorder {
    answer: Answer,
    asked: AtomicUsize,
    seen: Mutex<Vec<Pending>>,
}

impl Recorder {
    fn new(answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            answer,
            asked: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        })
    }
}

impl Approver for Recorder {
    fn ask(&self, _: &HostEnv, p: &Pending, _: Duration) -> Option<Answer> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(p.clone());
        Some(self.answer)
    }
}

fn ceiling(parent: &str, depth: u32, project: &Path) -> Ceiling {
    Ceiling {
        parent: parent.into(),
        parent_harness: "claude".into(),
        chain: vec![parent.into()],
        depth,
        repo: project.to_path_buf(),
        head_from: project.to_path_buf(),
        backend: "bwrap".into(),
        network: Network::None,
        lan: false,
        inherit_env: false,
        env_names: BTreeSet::new(),
        allow_domains: BTreeSet::new(),
        host_ports: BTreeSet::new(),
        mounts: vec![],
        manifest: Manifest::default(),
        modes: HostPolicy::default().modes,
        auth: "shared".into(),
    }
}

struct Setup {
    _tmp: tempfile::TempDir,
    env: HostEnv,
    project: PathBuf,
}

fn setup() -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let env = env_in(tmp.path());
    let project = tmp.path().join("work/app");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("task.md"), SECRET_TASK).unwrap();
    Setup {
        _tmp: tmp,
        env,
        project,
    }
}

fn broker(
    s: &Setup,
    id: &str,
    chain: Vec<String>,
    cfg: SpawnCfg,
    program: &[&str],
    approver: Arc<Recorder>,
) -> Broker {
    let depth = chain.len() as u32;
    let ctx = Context {
        id: id.into(),
        harness: "claude".into(),
        project: s.project.clone(),
        repo: None,
        chain,
        depth,
    };
    let mut b = Broker::new(s.env.clone(), ctx, HostPolicy::default(), approver);
    let sp = Spawner::with_program(
        &s.env,
        cfg,
        ceiling(id, depth, &s.project),
        &s.project,
        program.iter().map(OsString::from).collect(),
    );
    b.spawner = Some(Arc::new(sp));
    b
}

fn req(args: serde_json::Value) -> Request {
    Request {
        v: PROTOCOL,
        cap: "spawn".into(),
        args: args
            .as_object()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect(),
        reason: "split the work".into(),
    }
}

fn start(harness: &str) -> Request {
    req(serde_json::json!({ "action": "start", "harness": harness, "prompt_file": "task.md" }))
}

fn cfg(harnesses: &[&str], max_running: u32) -> SpawnCfg {
    SpawnCfg {
        harnesses: harnesses.iter().map(|h| h.to_string()).collect(),
        max_running,
        ..Default::default()
    }
}

#[test]
fn spawn_is_off_without_listed_harnesses_and_at_the_depth_limit() {
    let s = setup();
    let r = Recorder::new(Answer::Once);
    let b = broker(&s, "p-1", vec![], cfg(&[], 2), &["true"], r.clone());
    assert_eq!(b.mode("spawn"), Mode::Deny);
    assert_eq!(b.handle(&start("codex")).status, RStatus::Disabled);

    // A child (depth 1) with the default max_depth of 1 cannot spawn, even
    // when its harness list allows it. The depth comes from the host context.
    let child = broker(
        &s,
        "codex-spawn-1",
        vec!["p-1".into()],
        cfg(&["codex"], 2),
        &["true"],
        r.clone(),
    );
    assert_eq!(child.mode("spawn"), Mode::Deny);
    let resp = child.handle(&start("codex"));
    assert_eq!(resp.status, RStatus::Disabled);
    assert!(resp.message.contains("max_depth"), "{}", resp.message);
    let caps = child.capabilities();
    assert_eq!(caps["started_by"], "p-1");
    assert!(caps["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["name"] == "spawn" && c["mode"] == "deny"));

    // A harness that is not listed is refused before anybody is asked.
    let b = broker(
        &s,
        "p-2",
        vec![],
        cfg(&["opencode"], 2),
        &["true"],
        r.clone(),
    );
    let resp = b.handle(&start("codex"));
    assert_eq!(resp.status, RStatus::Disabled);
    assert!(resp.message.contains("[spawn] harnesses"));
    assert_eq!(r.asked.load(Ordering::SeqCst), 0);
}

#[test]
fn max_running_is_enforced_on_the_host() {
    let s = setup();
    let r = Recorder::new(Answer::Once);
    let b = broker(
        &s,
        "p-1",
        vec![],
        cfg(&["codex"], 1),
        &["sh", "-c", "sleep 20", "job"],
        r.clone(),
    );
    let first = b.handle(&start("codex"));
    assert_eq!(first.status, RStatus::Done, "{}", first.message);
    let second = b.handle(&start("codex"));
    assert_eq!(second.status, RStatus::Failed);
    assert!(second.message.contains("max_running"), "{}", second.message);
    // The spawner checks again under its own lock.
    let sp = b.spawner.clone().unwrap();
    assert!(sp.start("codex", b"x").is_err());
    let id = first.result["id"].as_str().unwrap().to_string();
    let killed = b.handle(&req(serde_json::json!({ "action": "kill", "id": id })));
    assert_eq!(killed.status, RStatus::Done);
    assert_eq!(killed.result["state"], "stopped");
    assert_eq!(sp.running(), 0);
}

#[test]
fn a_spawn_grant_covers_one_harness_and_never_crosses_to_a_child() {
    let s = setup();
    let r = Recorder::new(Answer::Session);
    let parent = broker(
        &s,
        "p-1",
        vec![],
        cfg(&["codex", "opencode"], 2),
        &["true"],
        r.clone(),
    );
    let sp = parent.spawner.clone().unwrap();
    let run = |b: &Broker, h: &str| {
        let resp = b.handle(&start(h));
        assert_eq!(resp.status, RStatus::Done, "{}", resp.message);
        let id = resp.result["id"].as_str().unwrap().to_string();
        sp.wait(&id, Duration::from_secs(10));
    };
    run(&parent, "codex");
    assert_eq!(r.asked.load(Ordering::SeqCst), 1);
    // The card shows the harness, the login and the task.
    let card = r.seen.lock().unwrap()[0].clone();
    assert!(card.detail.contains("codex"));
    assert!(card.detail.contains("Login: shared"));
    assert!(card.detail.contains(SECRET_TASK));
    assert!(card.session_allowed);
    run(&parent, "codex");
    assert_eq!(
        r.asked.load(Ordering::SeqCst),
        1,
        "the session grant covers codex"
    );
    run(&parent, "opencode");
    assert_eq!(r.asked.load(Ordering::SeqCst), 2, "but not another harness");

    // Grants of the parent never reach a child, in either direction.
    let exec = Request {
        v: PROTOCOL,
        cap: "exec".into(),
        args: [("argv".to_string(), serde_json::json!(["true"]))]
            .into_iter()
            .collect(),
        reason: String::new(),
    };
    assert_eq!(parent.handle(&exec).status, RStatus::Done);
    assert_eq!(parent.handle(&exec).status, RStatus::Done);
    assert_eq!(r.asked.load(Ordering::SeqCst), 3);
    let child = broker(
        &s,
        "codex-spawn-1",
        vec!["p-1".into()],
        cfg(&[], 2),
        &["true"],
        r.clone(),
    );
    assert_eq!(child.handle(&exec).status, RStatus::Done);
    assert_eq!(
        r.asked.load(Ordering::SeqCst),
        4,
        "the child is asked again"
    );
    assert_eq!(child.handle(&exec).status, RStatus::Done);
    assert_eq!(r.asked.load(Ordering::SeqCst), 4);
    let other = broker(&s, "p-1", vec![], cfg(&[], 2), &["true"], r.clone());
    assert_eq!(other.handle(&exec).status, RStatus::Done);
    assert_eq!(
        r.asked.load(Ordering::SeqCst),
        5,
        "a child's grant is not the parent's"
    );
}

#[test]
fn a_childs_card_shows_who_started_it() {
    let s = setup();
    let r = Recorder::new(Answer::Deny);
    let child = broker(
        &s,
        "codex-spawn-2",
        vec!["claude-app-1".into(), "codex-spawn-1".into()],
        cfg(&[], 2),
        &["true"],
        r.clone(),
    );
    let exec = Request {
        v: PROTOCOL,
        cap: "exec".into(),
        args: [("argv".to_string(), serde_json::json!(["ls"]))]
            .into_iter()
            .collect(),
        reason: String::new(),
    };
    assert_eq!(child.handle(&exec).status, RStatus::Denied);
    let p = r.seen.lock().unwrap()[0].clone();
    assert_eq!(p.started_by, vec!["claude-app-1", "codex-spawn-1"]);
    let note = crate::broker::approval_note(&p);
    assert!(
        note.body
            .starts_with("Started by sandbox codex-spawn-1. Chain: claude-app-1 > codex-spawn-1."),
        "{}",
        note.body
    );
    assert_eq!(chain_text(&[]), None);
    assert_eq!(chain_text(&["a".into()]).unwrap(), "Started by sandbox a.");
    // The pending file for the panel carries the chain.
    let json = serde_json::to_value(&p).unwrap();
    assert_eq!(json["started_by"][1], "codex-spawn-1");
    // A request from a sandbox that the user started has no chain field.
    let mut top = p.clone();
    top.started_by.clear();
    assert!(serde_json::to_value(&top)
        .unwrap()
        .get("started_by")
        .is_none());
}

#[test]
fn the_prompt_never_reaches_argv_or_the_audit_log_and_results_reach_the_inbox() {
    let s = setup();
    let seen = s.project.parent().unwrap().join("argv.txt");
    // The fake child records its arguments and its command line as the
    // kernel shows them to other users, then writes a result.
    let script = format!(
        "printf '%s\\n' \"$0\" \"$@\" > {out}; tr '\\0' ' ' < /proc/$$/cmdline >> {out}; \
         d=$(dirname \"$1\"); stat -c %a \"$d/prompt.md\" >> {out}; \
         printf '{{\"exit_code\":0,\"branch\":\"b\",\"base\":\"c\",\"commits\":2,\"uncommitted\":0,\"stat\":\" a | 1 +\",\"output\":\"all done\",\"output_truncated\":false,\"started\":1,\"ended\":2}}' > \"$d/result.json\"",
        out = seen.display()
    );
    let r = Recorder::new(Answer::Once);
    let b = broker(
        &s,
        "p-1",
        vec![],
        cfg(&["codex"], 2),
        &["sh", "-c", &script, "fake-child"],
        r.clone(),
    );
    let resp = b.handle(&start("codex"));
    assert_eq!(resp.status, RStatus::Done, "{}", resp.message);
    let id = resp.result["id"].as_str().unwrap().to_string();
    assert_eq!(resp.result["branch"], format!("paddock/spawn-codex-{id}"));
    let sp = b.spawner.clone().unwrap();
    let done = sp.wait(&id, Duration::from_secs(10)).unwrap();
    assert_eq!(done.state, State::Exited, "{}", done.message);
    assert_eq!(done.exit_code, Some(0));

    let argv = std::fs::read_to_string(&seen).unwrap();
    assert!(!argv.contains("TASK-TEXT"), "{argv}");
    assert!(argv.contains("job.json"), "{argv}");
    // The prompt waits for the child in a private file.
    assert!(argv.trim_end().ends_with("600"), "{argv}");

    let inbox = s.project.join(done.inbox.as_deref().unwrap());
    let result: serde_json::Value =
        serde_json::from_slice(&std::fs::read(inbox.join("result.json")).unwrap()).unwrap();
    assert_eq!(result["commits"], 2);
    assert_eq!(result["state"], "exited");
    assert_eq!(
        std::fs::read_to_string(inbox.join("output.txt")).unwrap(),
        "all done"
    );

    let log = std::fs::read_to_string(crate::broker::audit_file(&s.env)).unwrap();
    assert!(!log.contains("TASK-TEXT"), "{log}");
    let end = log
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["decision"] == "child ended")
        .unwrap();
    assert_eq!(end["parent"], "p-1");
    assert_eq!(end["child"], format!("codex-spawn-{id}"));
    assert_eq!(end["child_harness"], "codex");
    assert_eq!(end["exit_code"], 0);
    assert!(end["duration_s"].is_u64());
}

#[test]
fn a_prompt_file_is_read_safely() {
    let s = setup();
    let read = |p: &str| crate::broker::read_prompt(&s.project, p);
    assert_eq!(read("task.md").unwrap(), SECRET_TASK.as_bytes());
    let abs = s.project.join("task.md");
    assert!(read(&abs.display().to_string()).is_ok());
    // Outside the project, through a symlink, a FIFO, too big, or empty.
    assert!(read("/etc/hostname").is_err());
    assert!(read("../x").is_err());
    std::os::unix::fs::symlink("/etc/passwd", s.project.join("link.md")).unwrap();
    assert!(read("link.md").is_err());
    let fifo = s.project.join("fifo.md");
    let c = std::ffi::CString::new(fifo.display().to_string()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(read("fifo.md").is_err());
    std::fs::write(
        s.project.join("big.md"),
        vec![b'x'; MAX_PROMPT as usize + 1],
    )
    .unwrap();
    assert!(read("big.md").is_err());
    std::fs::write(s.project.join("empty.md"), "  \n").unwrap();
    assert!(read("empty.md").is_err());
    assert!(read("missing.md").is_err());
}

#[test]
fn stop_all_ends_every_child() {
    let s = setup();
    let r = Recorder::new(Answer::Once);
    let b = broker(
        &s,
        "p-1",
        vec![],
        cfg(&["codex"], 2),
        &["sh", "-c", "sleep 30", "job"],
        r,
    );
    for _ in 0..2 {
        assert_eq!(b.handle(&start("codex")).status, RStatus::Done);
    }
    let sp = b.spawner.clone().unwrap();
    assert_eq!(sp.running(), 2);
    sp.stop_all(Duration::from_secs(10));
    assert_eq!(sp.running(), 0);
    assert!(sp.list().iter().all(|c| c.state == State::Stopped));
}

#[test]
fn the_end_of_the_output_is_kept() {
    assert_eq!(tail("abc", 5), ("abc".into(), false));
    assert_eq!(tail("abcdef", 3), ("def".into(), true));
    // Never in the middle of a character.
    let (t, cut) = tail("aé", 1);
    assert!(cut && t.is_empty());
}

#[test]
fn ps_lists_children_under_their_parent() {
    let r = |id: &str, parent: Option<&str>, started: u64| state::Running {
        id: id.into(),
        harness: "h".into(),
        project: PathBuf::from("/home/demo/p"),
        backend: "bwrap".into(),
        network: "none".into(),
        gui: "off".into(),
        pid: 1,
        started,
        auth: String::new(),
        window_pid: None,
        warnings: vec![],
        host_pid: 1,
        parent: parent.map(str::to_string),
        spawn_id: None,
    };
    let list = vec![
        r("a", None, 1),
        r("b", None, 2),
        r("a1", Some("a"), 3),
        r("a1x", Some("a1"), 4),
        r("orphan", Some("gone"), 5),
    ];
    let ids: Vec<String> = state::tree_order(list.clone())
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, ["a", "a1", "a1x", "b", "orphan"]);
    assert_eq!(state::depth_in(&list, &list[3]), 2);
    let d: Vec<String> = state::descendants(&list, "a")
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(d.len(), 2);
    assert!(d.contains(&"a1x".to_string()));
}
