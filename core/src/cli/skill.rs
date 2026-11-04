use anyhow::Result;
use clap::Subcommand;
use paddock::env::HostEnv;
use std::path::Path;

#[derive(Subcommand)]
pub(crate) enum SkillCmd {
    /// Copy the skill into ~/.claude/skills and ~/.agents/skills on the host.
    Install,
}

/// Every file of the paddock-host skill, by its path in the skill folder.
/// `SKILL.md` comes first: `paddock skill` prints it.
pub(crate) const HOST_SKILL: &[(&str, &str)] = &[
    (
        "SKILL.md",
        include_str!("../../skills/paddock-host/SKILL.md"),
    ),
    (
        "references/devices.md",
        include_str!("../../skills/paddock-host/references/devices.md"),
    ),
    (
        "references/git-and-forges.md",
        include_str!("../../skills/paddock-host/references/git-and-forges.md"),
    ),
    (
        "references/kv.md",
        include_str!("../../skills/paddock-host/references/kv.md"),
    ),
    (
        "references/pastures.md",
        include_str!("../../skills/paddock-host/references/pastures.md"),
    ),
    (
        "references/spawn.md",
        include_str!("../../skills/paddock-host/references/spawn.md"),
    ),
    (
        "references/timers.md",
        include_str!("../../skills/paddock-host/references/timers.md"),
    ),
];

/// Write the whole paddock-host skill folder into `dir`. The reference files
/// of an older install go first, so that none of them stays behind.
pub(crate) fn install_skill(dir: &Path) -> Result<()> {
    let refs = dir.join("references");
    if refs.is_symlink() {
        std::fs::remove_file(&refs)?;
    } else if refs.is_dir() {
        std::fs::remove_dir_all(&refs)?;
    }
    for (rel, text) in HOST_SKILL {
        let file = dir.join(rel);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&file, text)?;
    }
    Ok(())
}

pub(crate) fn run(env: &HostEnv, cmd: Option<SkillCmd>) -> Result<i32> {
    match cmd {
        None => print!("{}", HOST_SKILL[0].1),
        Some(SkillCmd::Install) => {
            for base in [".claude/skills", ".agents/skills"] {
                let dir = env.home.join(base).join("paddock-host");
                install_skill(&dir)?;
                println!("Installed {}", dir.display());
            }
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Every file in skills/paddock-host is in HOST_SKILL, so that
    /// `paddock skill install` copies the whole folder.
    #[test]
    fn the_installed_skill_holds_every_file_of_the_folder() {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(root, &p, out);
                } else {
                    out.push(p.strip_prefix(root).unwrap().display().to_string());
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/paddock-host");
        let mut on_disk = Vec::new();
        walk(&root, &root, &mut on_disk);
        on_disk.sort();
        let mut listed: Vec<String> = HOST_SKILL.iter().map(|(p, _)| p.to_string()).collect();
        listed.sort();
        assert_eq!(listed, on_disk);
        assert_eq!(HOST_SKILL[0].0, "SKILL.md");
        for (rel, text) in HOST_SKILL {
            assert_eq!(*text, std::fs::read_to_string(root.join(rel)).unwrap());
        }
    }

    #[test]
    fn installing_the_skill_replaces_old_reference_files() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("paddock-host");
        std::fs::create_dir_all(dir.join("references")).unwrap();
        std::fs::write(dir.join("references/gone.md"), "old").unwrap();
        install_skill(&dir).unwrap();
        install_skill(&dir).unwrap();
        assert!(!dir.join("references/gone.md").exists());
        for (rel, text) in HOST_SKILL {
            assert_eq!(std::fs::read_to_string(dir.join(rel)).unwrap(), *text);
        }
    }
}
