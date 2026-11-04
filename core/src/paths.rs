//! Path identity when one spelling is canonical and another is not.
//!
//! macOS makes `/var`, `/etc` and `/tmp` symlinks. A path from the caller
//! and a path from `canonicalize` or git then name one directory and still
//! compare unequal. These helpers treat those as the same file. On a system
//! where the path is not a symlink, the result matches a plain comparison.

use std::path::{Path, PathBuf};

/// True when both paths name the same file.
pub fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (canonicalize_existing(a), canonicalize_existing(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// True when `path` is `root` or a file inside it.
pub fn within(path: &Path, root: &Path) -> bool {
    if path == root || path.starts_with(root) {
        return true;
    }
    match (canonicalize_existing(path), canonicalize_existing(root)) {
        (Some(p), Some(r)) => p == r || p.starts_with(&r),
        _ => false,
    }
}

/// True when `path` is strictly inside `root`.
pub fn strictly_inside(path: &Path, root: &Path) -> bool {
    if path == root || same_file(path, root) {
        return false;
    }
    within(path, root)
}

/// Rewrite `path` into the spelling of `anchor` when they share a directory.
///
/// Git and `canonicalize` return the physical path. Callers keep the path
/// they passed in. `/var/folders/.../repo` as the anchor turns
/// `/private/var/folders/.../repo/.git` back into `/var/folders/.../repo/.git`.
pub fn spelling_of(anchor: &Path, path: &Path) -> PathBuf {
    let Some(real) = canonicalize_existing(path) else {
        return path.to_path_buf();
    };
    let mut current = anchor.to_path_buf();
    loop {
        if let Some(canon) = canonicalize_existing(&current) {
            if real == canon {
                return current;
            }
            if let Ok(rest) = real.strip_prefix(&canon) {
                return current.join(rest);
            }
        }
        if !current.pop() {
            break;
        }
    }
    path.to_path_buf()
}

/// `canonicalize`, or the same walk with a missing tail left in place.
fn canonicalize_existing(path: &Path) -> Option<PathBuf> {
    if let Ok(c) = std::fs::canonicalize(path) {
        return Some(c);
    }
    let mut missing = Vec::new();
    let mut cur = path.to_path_buf();
    while !cur.exists() {
        missing.push(cur.file_name()?.to_os_string());
        if !cur.pop() {
            return None;
        }
    }
    let mut out = std::fs::canonicalize(&cur).ok()?;
    for name in missing.into_iter().rev() {
        out.push(name);
    }
    Some(out)
}

/// True when one path in `paths` names the same file as `path`.
pub fn any_same<'a, I>(paths: I, path: &Path) -> bool
where
    I: IntoIterator<Item = &'a Path>,
{
    paths.into_iter().any(|p| same_file(p, path))
}

/// The first item whose path names the same file as `path`.
pub fn find_same<'a, T, I>(items: I, path: &Path, of: impl Fn(&T) -> &Path) -> Option<&'a T>
where
    I: IntoIterator<Item = &'a T>,
{
    items.into_iter().find(|item| same_file(of(item), path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_symlink_keeps_the_anchor_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("f"), b"x").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let anchor = link.join("f");
        let physical = std::fs::canonicalize(&anchor).unwrap();
        assert_ne!(anchor, physical);
        assert!(same_file(&anchor, &physical));
        assert!(within(&physical, &link));
        assert!(!strictly_inside(&physical, &link.join("f")));
        assert_eq!(spelling_of(&anchor, &physical), anchor);
        assert_eq!(
            spelling_of(&link, &physical.parent().unwrap().join("f")),
            anchor
        );
    }
}
