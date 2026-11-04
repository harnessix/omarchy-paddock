use crate::spec::MountKind;
use std::path::PathBuf;

/// Turn every writable mount below one of `roots` into a read-only one.
pub fn make_read_only(spec: &mut crate::spec::SandboxSpec, roots: &[PathBuf]) {
    for m in spec.mounts.iter_mut() {
        let Some(src) = m.src.clone() else {
            continue;
        };
        let Some(root) = roots.iter().find(|r| crate::paths::within(&src, r)) else {
            continue;
        };
        // The mount keeps the spelling of the root the caller named. On macOS
        // the spec otherwise holds `/private/var` while the caller said `/var`.
        let spelled = if crate::paths::same_file(&src, root) {
            root.clone()
        } else {
            crate::paths::spelling_of(root, &src)
        };
        if m.dst == src {
            m.dst = spelled.clone();
        }
        m.src = Some(spelled);
        if m.kind == MountKind::Rw {
            m.kind = MountKind::Ro;
            m.origin = format!("{} (read-only: reviewer)", m.origin);
        }
    }
}
