use anyhow::Result;
use paddock::env::HostEnv;

pub(crate) fn update_cmd(env: &HostEnv, check: bool, yes: bool) -> Result<i32> {
    use paddock::update::{self, Source};
    use std::io::{IsTerminal, Write};
    update::refuse_in_sandbox(env, "paddock update")?;
    let gh = update::GitHub::from_env(env, std::time::Duration::from_secs(30));
    let rel = gh.latest()?;
    // macOS: no release binary to install, and no Omarchy plugin.
    if let Some(line) = paddock::onboard::refusal(
        paddock::onboard::Os::HOST,
        paddock::onboard::Feature::UpdateBinary,
    ) {
        return Ok(mac_update(&rel.version, check, line));
    }
    let target = update::current_binary()?;
    let owner = update::package_owner(env, &target);
    let plugin = update::plugin_installed(env) == Some(true);
    let newer = update::is_newer(&rel.version, update::CURRENT);
    if newer {
        println!(
            "paddock {} is out; this is {}.",
            rel.version,
            update::CURRENT
        );
    } else {
        println!("paddock {} is the latest release.", update::CURRENT);
    }
    if newer {
        match &owner {
            Some(o) => println!("{}", update::package_hint(o, &rel.version)),
            None => println!(
                "An update downloads {} and its .sha256 from GitHub into a private folder, checks the checksum and the version of the new binary, and then replaces {}.",
                rel.tarball(),
                target.display()
            ),
        }
    }
    if plugin {
        println!(
            "The bar widget is installed. An update also runs: omarchy plugin update {} --yes",
            update::PLUGIN_ID
        );
    }
    let binary = newer && owner.is_none();
    // A package-owned binary that is out of date: refused, see the hint.
    let code = i32::from(newer && owner.is_some());
    if check || (!binary && !plugin) {
        return Ok(if check { 0 } else { code });
    }
    if !yes {
        let tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
        if !tty {
            println!("Run `paddock update --yes` to update.");
            return Ok(0);
        }
        eprint!("Update now? [y/N] ");
        std::io::stderr().flush().ok();
        let mut a = String::new();
        std::io::stdin().read_line(&mut a)?;
        if !matches!(a.trim(), "y" | "Y" | "yes") {
            println!("Nothing changed.");
            return Ok(1);
        }
    }
    if binary {
        let tar = env
            .which("tar")
            .ok_or_else(|| anyhow::anyhow!("paddock update needs tar"))?;
        update::install(env, &gh as &dyn Source, &rel, &target, None, &tar)?;
        println!("Updated {} to paddock {}.", target.display(), rel.version);
        println!("Running sandboxes keep the old version until they exit.");
        // The new version may bring profiles: its menu entries and shims.
        let _ = std::process::Command::new(&target)
            .arg("refresh")
            .stdin(std::process::Stdio::null())
            .status();
    }
    if plugin {
        update::update_plugin(env)?;
        println!("Updated the bar widget.");
    }
    Ok(code)
}

/// `paddock update` on macOS: says whether a newer release is out and how
/// to build it. It never downloads a binary.
fn mac_update(latest: &str, check: bool, line: &str) -> i32 {
    use paddock::update;
    if !update::is_newer(latest, update::CURRENT) {
        println!("paddock {} is the latest release.", update::CURRENT);
        return 0;
    }
    println!("paddock {latest} is out; this is {}.", update::CURRENT);
    let brew = update::current_binary().is_ok_and(|p| homebrew_owned(&p));
    if brew {
        println!("Homebrew installed paddock. Update it with: brew upgrade paddock");
    } else {
        println!("{line}");
    }
    i32::from(!check)
}

pub(crate) fn run(env: &HostEnv, check: bool, yes: bool) -> Result<i32> {
    update_cmd(env, check, yes)
}

/// True when Homebrew installed `path`: the binary is in a `Cellar` folder,
/// such as /opt/homebrew/Cellar/paddock/0.2.0/bin/paddock.
fn homebrew_owned(path: &std::path::Path) -> bool {
    path.components().any(|c| c.as_os_str() == "Cellar")
}

#[cfg(test)]
mod tests {
    use super::homebrew_owned;
    use std::path::Path;

    #[test]
    fn a_binary_in_a_homebrew_cellar_is_homebrew_owned() {
        assert!(homebrew_owned(Path::new(
            "/opt/homebrew/Cellar/paddock/0.2.0/bin/paddock"
        )));
        assert!(homebrew_owned(Path::new(
            "/usr/local/Cellar/paddock/0.2.0/bin/paddock"
        )));
        assert!(!homebrew_owned(Path::new("/opt/homebrew/bin/paddock")));
        assert!(!homebrew_owned(Path::new("/Users/a/.cargo/bin/paddock")));
    }
}
