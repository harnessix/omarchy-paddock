//! Omarchy's usage collector for a kept login, in the keeper's sandbox.

use super::*;

/// Omarchy's usage collector for a kept login, in a keeper sandbox, with the
/// full login. The collector starts the harness's own server, which can
/// refresh the login; then the result goes back where the login lives and
/// into the running sandboxes, as after a refresh. Returns its output.
pub fn collect_usage(
    env: &HostEnv,
    harness: &str,
    store: &Path,
    collector: &Path,
    args: &[String],
) -> Result<Vec<u8>> {
    let kind = kind_for(harness).context("not a kept login")?;
    let h = crate::profile::HarnessFactory::new(env)?.create(harness)?;
    let _lock = refresh_lock(env, store)?;
    let before = std::fs::read(store)?;
    if !keepable(&before) {
        bail!("{} holds no login with a refresh token", store.display());
    }
    let dir = keeper_dir(env, store);
    let (upper, work) = (dir.join("usage-upper"), dir.join("usage-work"));
    for d in [&upper, &work] {
        let _ = std::fs::remove_dir_all(d);
        crate::privfs::dir(d)?;
    }
    let login = upper.join(Path::new(kind.file).file_name().context("no file name")?);
    crate::privfs::write(&login, &before)?;
    // A shared login only: the overlay's lower layer is the host's folder.
    let mut cmd = keeper_command(
        env,
        &h,
        kind,
        &AuthMode::Shared,
        &dir,
        Layout::Collector {
            upper: &upper,
            work: &work,
            program: collector,
            args,
        },
    )?;
    cmd.env_remove(kind.home_var);
    let out = run_capped(&mut cmd, Duration::from_secs(120))?;
    let after = std::fs::read(&login).unwrap_or_default();
    if after != before && keepable(&after) {
        let now = std::fs::read(store).unwrap_or_default();
        if hash(&now) == hash(&before) {
            write_store(store, &after)?;
        }
        push(env, store)?;
    }
    let _ = std::fs::remove_dir_all(&work);
    Ok(out)
}

/// Run a command, return its standard output (at most 4 MiB), and stop it
/// after `wait`.
fn run_capped(cmd: &mut Command, wait: Duration) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot start the keeper sandbox")?;
    let mut out = child.stdout.take().context("no stdout")?;
    let mut err = child.stderr.take().context("no stderr")?;
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut err).take(64 << 10).read_to_end(&mut buf);
        buf
    });
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut out).take(4 << 20).read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if start.elapsed() > wait {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the collector did not finish in {} seconds", wait.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    if out.is_empty() && !err.is_empty() {
        // The collector's own words, for `PADDOCK_LOG=debug`. Never a token:
        // a collector does not print one.
        log::debug!(
            "the collector printed nothing; its errors: {}",
            String::from_utf8_lossy(&err).trim()
        );
    }
    Ok(out)
}
