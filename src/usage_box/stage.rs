//! Overlays with more layers than one mount can take.
//!
//! bwrap passes all lower layers of an overlay in one mount option string,
//! and the kernel takes at most one page for it. With many sandbox homes,
//! the paths of one harness folder do not fit. bwrap takes only host paths
//! as layers, so the box cannot first bind them to short paths of its own.
//!
//! So the merge has two steps. An outer bwrap (the stage) sees the host
//! read-only and mounts groups of layers that each fit, every group as a
//! read-only overlay at a short path: an empty folder in paddock's runtime
//! folder for this update. In the stage, the box's own bwrap then builds the
//! harness folder from these short paths. The stage mounts live only in its
//! mount namespace: the host sees the empty folders, nothing else.

use super::{Mount, View, LOWER_BUDGET};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// One group of layers that the stage merges at `dest`, from the bottom
/// to the top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub dest: PathBuf,
    pub lowers: Vec<PathBuf>,
}

/// What one layer costs in the mount option string: bwrap adds `/oldroot`
/// and a separator, and escapes a few characters.
pub fn cost(p: &Path) -> usize {
    p.as_os_str().len() + 10
}

fn total(lowers: &[PathBuf]) -> usize {
    lowers.iter().map(|p| cost(p)).sum()
}

/// Split the overlays that do not fit into groups at short paths under
/// `dir`, and point the overlays at the groups. When even the groups do not
/// fit, the bottom (oldest) groups are left out and counted in
/// `view.left_out`.
pub fn split(view: &mut View, dir: &Path) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for m in view.mounts.iter_mut() {
        let Mount::Overlay { lowers, .. } = m else {
            continue;
        };
        if total(lowers) <= LOWER_BUDGET {
            continue;
        }
        let mut chunks: Vec<Vec<PathBuf>> = Vec::new();
        let mut used = 0;
        for l in lowers.drain(..) {
            match chunks.last_mut() {
                Some(c) if used + cost(&l) <= LOWER_BUDGET => {
                    used += cost(&l);
                    c.push(l);
                }
                _ => {
                    used = cost(&l);
                    chunks.push(vec![l]);
                }
            }
        }
        // Keep the top groups whose short paths fit, from the top down.
        let short = cost(&dir.join("0000"));
        let keep_from = chunks.len().saturating_sub(LOWER_BUDGET / short);
        for (i, chunk) in chunks.into_iter().enumerate() {
            if i < keep_from {
                view.left_out += chunk.len();
                continue;
            }
            let dest = dir.join(groups.len().to_string());
            lowers.push(dest.clone());
            groups.push(Group {
                dest,
                lowers: chunk,
            });
        }
    }
    groups
}

/// Without a stage: keep the newest layers that fit in one mount and count
/// the rest in `view.left_out`. The top layer (the host folder) stays.
pub fn trim(view: &mut View) {
    for m in view.mounts.iter_mut() {
        let Mount::Overlay { lowers, .. } = m else {
            continue;
        };
        let mut used = 0;
        let mut keep = lowers.len();
        while keep > 0 && used + cost(&lowers[keep - 1]) <= LOWER_BUDGET {
            used += cost(&lowers[keep - 1]);
            keep -= 1;
        }
        view.left_out += keep;
        lowers.drain(..keep);
    }
}

/// The stage's bwrap arguments before the box's own bwrap command: the host
/// read-only, `/proc` as it is (the box mounts its own), and the groups.
pub fn args(groups: &[Group]) -> Vec<String> {
    let s = |p: &Path| p.display().to_string();
    let mut a: Vec<String> = [
        "--die-with-parent",
        "--unshare-user",
        "--ro-bind",
        "/",
        "/",
        "--bind",
        "/proc",
        "/proc",
    ]
    .map(String::from)
    .to_vec();
    for g in groups {
        if g.lowers.len() == 1 {
            a.extend(["--ro-bind".into(), s(&g.lowers[0]), s(&g.dest)]);
            continue;
        }
        for l in &g.lowers {
            a.extend(["--overlay-src".into(), s(l)]);
        }
        a.extend(["--ro-overlay".into(), s(&g.dest)]);
    }
    a.push("--".into());
    a
}

/// Make the empty mount points of the groups.
pub fn prepare(groups: &[Group]) -> Result<()> {
    for g in groups {
        crate::privfs::dir(&g.dest)?;
    }
    Ok(())
}

/// Whether a stage works here: an overlay in the stage, used as a layer by
/// a box inside it.
pub fn probe(bwrap: &Path, dir: &Path) -> Result<()> {
    let g = Group {
        dest: dir.join("probe"),
        lowers: vec!["/usr".into(), "/etc".into()],
    };
    prepare(std::slice::from_ref(&g))?;
    let mut a = args(std::slice::from_ref(&g));
    a.push(bwrap.display().to_string());
    a.extend(
        [
            "--unshare-user",
            "--unshare-pid",
            "--disable-userns",
            "--ro-bind",
            "/",
            "/",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--overlay-src",
        ]
        .map(String::from),
    );
    a.push(g.dest.display().to_string());
    a.extend(["--tmp-overlay", "/tmp/o", "true"].map(String::from));
    let ok = Command::new(bwrap)
        .args(&a)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let _ = std::fs::remove_dir(&g.dest);
    if !ok {
        bail!("bwrap cannot merge layers in two steps here");
    }
    Ok(())
}
