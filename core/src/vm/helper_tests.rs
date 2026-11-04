use super::*;
use crate::vm::assets::tests::fake_assets;

fn parse(args: &[&str]) -> Result<StartArgs, clap::Error> {
    let mut argv = vec!["paddock-vm", "start"];
    argv.extend_from_slice(args);
    Cli::try_parse_from(argv).map(|c| match c.cmd {
        Cmd::Start(a) => a,
    })
}

/// A fake home with a project, the assets and a state folder.
struct Fixture {
    _t: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    assets: PathBuf,
    state: PathBuf,
}

fn fixture() -> Fixture {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().canonicalize().unwrap();
    let home = root.join("home");
    let project = home.join("src/app");
    let assets = home.join(".local/share/paddock/vm");
    let state = home.join(".local/state/paddock/vm/run1");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&assets).unwrap();
    fake_assets(&assets, false);
    Fixture {
        _t: t,
        home,
        project,
        assets,
        state,
    }
}

fn args_for(f: &Fixture, project: &Path, extra: &[&str]) -> StartArgs {
    let (a, p, s) = (
        f.assets.display().to_string(),
        project.display().to_string(),
        f.state.display().to_string(),
    );
    let mut argv = vec!["--assets", &a, "--project", &p, "--state", &s];
    argv.extend_from_slice(extra);
    parse(&argv).unwrap()
}

#[test]
fn start_parses_all_arguments() {
    let a = parse(&[
        "--assets",
        "/a",
        "--project",
        "/p",
        "--state",
        "/s",
        "--cpus",
        "4",
        "--memory",
        "4096",
        "--forward",
        "1025=/tmp/b.sock",
        "--forward",
        "proxy=/tmp/p.sock",
        "--append",
        "init=/bin/true",
        "--stop-timeout",
        "3",
    ])
    .unwrap();
    assert_eq!((a.cpus, a.memory, a.stop_timeout), (4, 4096, 3));
    assert_eq!(
        a.forwards,
        vec![
            (1025, PathBuf::from("/tmp/b.sock")),
            (1026, PathBuf::from("/tmp/p.sock"))
        ]
    );
    assert_eq!(a.append, vec!["init=/bin/true"]);
    let d = parse(&["--assets", "/a", "--project", "/p", "--state", "/s"]).unwrap();
    assert_eq!((d.cpus, d.memory, d.stop_timeout), (2, 2048, 10));
}

#[test]
fn start_refuses_bad_arguments() {
    let base = ["--assets", "/a", "--project", "/p", "--state", "/s"];
    for extra in [
        &["--cpus", "0"][..],
        &["--cpus", "65"],
        &["--memory", "64"],
        &["--forward", "22=/tmp/x.sock"],
        &["--forward", "1025"],
        &["--forward", "1025=relative.sock"],
        &["--stop-timeout", "0"],
    ] {
        let mut argv = base.to_vec();
        argv.extend_from_slice(extra);
        assert!(parse(&argv).is_err(), "{extra:?}");
    }
    let long = format!("1025=/{}", "x".repeat(200));
    let mut argv = base.to_vec();
    argv.extend_from_slice(&["--forward", &long]);
    assert!(parse(&argv).is_err());
    assert!(parse(&["--assets", "/a", "--project", "/p"]).is_err());
}

#[test]
fn plan_checks_and_collects_everything() {
    let f = fixture();
    let a = args_for(
        &f,
        &f.project,
        &["--forward", "broker=/tmp/b.sock", "--memory", "1024"],
    );
    let p = plan(&a, &f.home).unwrap();
    assert_eq!(p.project, f.project);
    assert_eq!(p.state.dir, f.state);
    assert_eq!(p.memory_bytes, 1024 << 20);
    assert_eq!(
        p.forwards.get(&ports::BROKER),
        Some(&PathBuf::from("/tmp/b.sock"))
    );
    assert_eq!(
        p.cmdline,
        "console=hvc0 root=/dev/vda rootfstype=ext4 ro init=/sbin/paddock-init"
    );
    assert_eq!(p.assets.kernel, f.assets.join("kernel"));
    assert_eq!(p.stop_timeout, Duration::from_secs(10));
    assert_eq!(p.state.control, f.state.join("control.sock"));
}

#[test]
fn plan_takes_a_short_control_socket_and_refuses_a_long_one() {
    let f = fixture();
    let a = args_for(&f, &f.project, &["--control", "/tmp/p-501/vm/control.sock"]);
    let p = plan(&a, &f.home).unwrap();
    assert_eq!(p.state.control, Path::new("/tmp/p-501/vm/control.sock"));
    // The state folder of a temporary home on macOS is too long for it.
    let long = format!("/{}/control.sock", "x".repeat(110));
    let a = args_for(&f, &f.project, &["--control", &long]);
    let e = plan(&a, &f.home).unwrap_err().to_string();
    assert!(e.contains("byte limit"), "{e}");
    let a = args_for(&f, &f.project, &["--control", "control.sock"]);
    assert!(plan(&a, &f.home).is_err());
}

#[test]
fn plan_refuses_the_home_and_its_parents() {
    let f = fixture();
    for project in [
        f.home.clone(),
        f.home.parent().unwrap().to_path_buf(),
        PathBuf::from("/"),
    ] {
        let e = plan(&args_for(&f, &project, &[]), &f.home)
            .unwrap_err()
            .to_string();
        assert!(e.contains("contains it"), "{project:?}: {e}");
    }
    // The same through a symlink and a `..` path.
    let link = f.project.join("../../../home-link");
    std::os::unix::fs::symlink(&f.home, &link).unwrap();
    assert!(plan(&args_for(&f, &link, &[]), &f.home).is_err());
    let dots = f.project.join("../..");
    assert!(plan(&args_for(&f, &dots, &[]), &f.home).is_err());
}

#[test]
fn plan_refuses_a_missing_project_or_asset() {
    let f = fixture();
    let missing = f.home.join("nope");
    let e = plan(&args_for(&f, &missing, &[]), &f.home)
        .unwrap_err()
        .to_string();
    assert!(e.contains("missing"), "{e}");
    std::fs::write(f.assets.join("rootfs.img"), "changed!!!!!!!!").unwrap();
    let e = plan(&args_for(&f, &f.project, &[]), &f.home)
        .unwrap_err()
        .to_string();
    assert!(e.contains("rootfs.img"), "{e}");
}

#[test]
fn plan_keeps_state_and_assets_out_of_the_project() {
    let f = fixture();
    let mut a = args_for(&f, &f.project, &[]);
    a.state = f.project.join(".vm");
    let e = plan(&a, &f.home).unwrap_err().to_string();
    assert!(e.contains("state folder"), "{e}");
    let inner = f.project.join("vm");
    std::fs::create_dir_all(&inner).unwrap();
    fake_assets(&inner, false);
    let mut a = args_for(&f, &f.project, &[]);
    a.assets = inner;
    let e = plan(&a, &f.home).unwrap_err().to_string();
    assert!(e.contains("assets folder"), "{e}");
}

#[test]
fn plan_refuses_a_port_twice() {
    let f = fixture();
    let a = args_for(
        &f,
        &f.project,
        &[
            "--forward",
            "1025=/tmp/a.sock",
            "--forward",
            "broker=/tmp/b.sock",
        ],
    );
    assert!(plan(&a, &f.home).unwrap_err().to_string().contains("two"));
}

#[test]
fn kernel_cmdline_adds_safe_arguments_only() {
    let extra = vec!["init=/bin/true".to_string(), "panic=-1".into()];
    assert_eq!(
        kernel_cmdline(&extra).unwrap(),
        "console=hvc0 root=/dev/vda rootfstype=ext4 ro init=/sbin/paddock-init init=/bin/true panic=-1"
    );
    for bad in ["", "a b", "a\nb", "a\tb"] {
        assert!(kernel_cmdline(&[bad.to_string()]).is_err(), "{bad:?}");
    }
}

#[test]
fn state_layout_names_the_files() {
    let s = StateLayout::new(Path::new("/s"));
    assert_eq!(s.machine_id, Path::new("/s/machine-id"));
    assert_eq!(s.disk, Path::new("/s/disk.img"));
    assert_eq!(s.console, Path::new("/s/console.log"));
    assert_eq!(s.control, Path::new("/s/control.sock"));
}

#[test]
fn prepare_makes_a_private_folder_and_a_sparse_disk() {
    let t = tempfile::tempdir().unwrap();
    let s = StateLayout::new(&t.path().join("a/b"));
    s.prepare().unwrap();
    let mode = std::fs::metadata(&s.dir).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    assert!(s.ensure_disk(STATE_DISK_BYTES).unwrap());
    let m = std::fs::metadata(&s.disk).unwrap();
    assert_eq!(m.len(), STATE_DISK_BYTES);
    use std::os::unix::fs::MetadataExt;
    // Sparse: almost no blocks are in use.
    assert!(m.blocks() * 512 < 1 << 20, "{} blocks", m.blocks());
    assert_eq!(m.permissions().mode() & 0o777, 0o600);
    // A second start keeps the disk.
    std::fs::write(&s.disk, "guest data").unwrap();
    assert!(!s.ensure_disk(STATE_DISK_BYTES).unwrap());
    assert_eq!(std::fs::read(&s.disk).unwrap(), b"guest data");
}

#[test]
fn prepare_refuses_a_file_as_the_folder() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("state");
    std::fs::write(&path, "").unwrap();
    assert!(StateLayout::new(&path).prepare().is_err());
    let link = t.path().join("link");
    std::os::unix::fs::symlink(t.path(), &link).unwrap();
    assert!(StateLayout::new(&link).prepare().is_err());
}

#[test]
fn machine_id_is_written_once_and_read_back() {
    let t = tempfile::tempdir().unwrap();
    let s = StateLayout::new(t.path());
    assert_eq!(s.read_machine_id().unwrap(), None);
    s.write_machine_id(b"id one").unwrap();
    assert_eq!(
        s.read_machine_id().unwrap().as_deref(),
        Some(&b"id one"[..])
    );
    assert!(s.write_machine_id(b"id two").is_err());
    assert_eq!(
        s.read_machine_id().unwrap().as_deref(),
        Some(&b"id one"[..])
    );
    let mode = std::fs::metadata(&s.machine_id)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(s.write_machine_id(b"").is_err());
}

#[test]
fn machine_id_refuses_a_damaged_file() {
    let t = tempfile::tempdir().unwrap();
    let s = StateLayout::new(t.path());
    std::fs::write(&s.machine_id, "").unwrap();
    assert!(s
        .read_machine_id()
        .unwrap_err()
        .to_string()
        .contains("damaged"));
    std::fs::write(&s.machine_id, vec![0u8; 5000]).unwrap();
    assert!(s.read_machine_id().is_err());
}
