//! The binds of the spec mounts. bwrap cannot mount onto a destination that
//! is a symlink in the sandbox. The Alpine guest of the VM backend has
//! `/usr/bin/sh` as a link to busybox, and the located harness program is a
//! mount at its own path, below the read-only `/usr`.

use super::ROOT_LINKS;
use crate::spec::{Mount, MountKind};
use std::path::{Path, PathBuf};

/// The host trees that the sandbox binds read-only at the same path.
pub(super) fn system_trees() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/usr")];
    v.extend(
        ROOT_LINKS
            .iter()
            .chain(["/opt", "/nix", "/gnu"].iter())
            .map(PathBuf::from)
            .filter(|p| p.exists()),
    );
    v
}

/// The source and destination of a bind mount, or None when the sandbox
/// shows the path already. A same-path mount whose path is a symlink
/// inside a bound tree binds the link target instead: the link stays,
/// so a program started through it keeps the link name as argv0.
pub(super) fn bind(m: &Mount, trees: &[PathBuf], all: &[&Mount]) -> Option<(PathBuf, PathBuf)> {
    let src = m.src.clone().unwrap_or_else(|| m.dst.clone());
    if src != m.dst {
        return Some((src, m.dst.clone()));
    }
    let in_tree = |p: &Path| trees.iter().any(|t| p.starts_with(t));
    // Another mount above the path, such as a mask, changes what shows there.
    let under_mount = |p: &Path| {
        all.iter()
            .any(|o| o.dst != m.dst && p.starts_with(&o.dst) && p != o.dst)
    };
    let shown = |p: &Path| m.kind == MountKind::Ro && in_tree(p) && !under_mount(p);
    if shown(&m.dst) {
        return None;
    }
    let is_link = std::fs::symlink_metadata(&m.dst).is_ok_and(|md| md.file_type().is_symlink());
    if is_link && (in_tree(&m.dst) || under_mount(&m.dst)) {
        if let Ok(real) = std::fs::canonicalize(&m.dst) {
            if shown(&real) {
                return None;
            }
            return Some((real.clone(), real));
        }
    }
    Some((src, m.dst.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> (tempfile::TempDir, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let bin = root.join("usr/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("busybox"), "").unwrap();
        std::os::unix::fs::symlink("busybox", bin.join("sh")).unwrap();
        (t, root)
    }

    fn no_bind_onto_a_link(args: &[(PathBuf, PathBuf)]) {
        for (_, dst) in args {
            let md = std::fs::symlink_metadata(dst);
            assert!(
                !md.is_ok_and(|m| m.file_type().is_symlink()),
                "bind onto the symlink {}",
                dst.display()
            );
        }
    }

    #[test]
    fn a_program_link_inside_a_bound_tree_is_not_bound() {
        let (_t, root) = tree();
        let trees = vec![root.join("usr")];
        let sh = root.join("usr/bin/sh");
        let m = Mount::same(&sh, false, "profile fake: install");
        assert_eq!(bind(&m, &trees, &[&m]), None);
    }

    #[test]
    fn a_link_below_a_mount_binds_its_target() {
        let (_t, root) = tree();
        let usr = Mount::same(root.join("usr"), true, "manifest mounts.rw");
        let m = Mount::same(root.join("usr/bin/sh"), false, "profile fake: install");
        let got: Vec<_> = [&usr, &m]
            .iter()
            .filter_map(|x| bind(x, &[], &[&usr, &m]))
            .collect();
        let busybox = root.join("usr/bin/busybox");
        assert!(got.contains(&(busybox.clone(), busybox)), "{got:?}");
        no_bind_onto_a_link(&got);
    }

    #[test]
    fn a_program_outside_the_bound_trees_is_bound_as_before() {
        let (_t, root) = tree();
        let sh = root.join("usr/bin/sh");
        let m = Mount::same(&sh, false, "profile fake: install");
        assert_eq!(bind(&m, &[], &[&m]), Some((sh.clone(), sh)));
        let other = Mount::bind("/a", "/b", false, "x");
        assert_eq!(
            bind(&other, &[], &[&other]),
            Some(("/a".into(), "/b".into()))
        );
    }
}
