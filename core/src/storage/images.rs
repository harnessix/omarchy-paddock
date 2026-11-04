use super::*;
use crate::env::HostEnv;
use crate::state;
use std::collections::{BTreeMap, BTreeSet};

/// "1.509GB", "807.8MB", "4.096kB", "98B": docker's decimal sizes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let i = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (num, unit) = s.split_at(i);
    let n: f64 = num.parse().ok()?;
    let mult = match unit.trim() {
        "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1048576.0,
        "GiB" => 1073741824.0,
        _ => return None,
    };
    Some((n * mult) as u64)
}

/// paddock's records in the docker build cache: their RUN lines carry the
/// marker, or the commands of base images built before it existed. Only the
/// bytes that nothing else shares count.
pub(super) fn scan_build_cache(host: &Host, cli: &str) -> Vec<BuildRecord> {
    use crate::backend::docker::{BUILD_MARKER, OLD_BUILD_COMMANDS};
    if cli != "docker" {
        return vec![];
    }
    let Ok(text) = host
        .containers
        .run(cli, &["buildx", "du", "--format", "json"])
    else {
        return vec![];
    };
    let marker = format!(": {BUILD_MARKER} ");
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["Reclaimable"].as_bool() == Some(true))
        .filter_map(|v| {
            let d = v["Description"].as_str().unwrap_or_default();
            let ours = d.contains(&marker) || OLD_BUILD_COMMANDS.iter().any(|c| d.contains(c));
            let id = v["ID"].as_str().unwrap_or_default();
            (ours && !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())).then(|| {
                BuildRecord {
                    cli: cli.to_string(),
                    id: id.to_string(),
                    bytes: if v["Shared"].as_bool() == Some(true) {
                        0
                    } else {
                        parse_size(v["Size"].as_str().unwrap_or("0B")).unwrap_or(0)
                    },
                    description: d.chars().take(120).collect(),
                }
            })
        })
        .collect()
}

/// The image table of `podman system df -v`: columns are separated by two
/// or more spaces; IMAGE ID is short. Returns (short id, unique bytes).
pub fn parse_podman_df(text: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut in_images = false;
    for line in text.lines() {
        if line.starts_with("Images space usage") {
            in_images = true;
            continue;
        }
        if in_images && line.trim().is_empty() && !out.is_empty() {
            break;
        }
        if !in_images || line.starts_with("REPOSITORY") || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line
            .split("  ")
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect();
        if cols.len() >= 8 {
            if let Some(u) = parse_size(cols[6]) {
                out.push((cols[2].to_string(), u));
            }
        }
    }
    out
}

/// Unique sizes of images, from `docker system df -v` or `podman system df -v`.
fn unique_sizes(host: &Host, cli: &str) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    if cli == "podman" {
        if let Ok(text) = host.containers.run(cli, &["system", "df", "-v"]) {
            out.extend(parse_podman_df(&text));
        }
        return out;
    }
    if cli != "docker" {
        return out;
    }
    if let Ok(text) = host
        .containers
        .run(cli, &["system", "df", "-v", "--format", "json"])
    {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            for i in v["Images"].as_array().into_iter().flatten() {
                if let (Some(id), Some(u)) = (
                    i["ID"].as_str(),
                    i["UniqueSize"].as_str().and_then(parse_size),
                ) {
                    out.insert(strip_sha(id).to_string(), u);
                }
            }
        }
    }
    out
}

/// The image tags that paddock would use now: the user configuration and
/// each recent project, with and without the desktop, app and keyring parts.
pub fn current_tags(env: &HostEnv) -> BTreeSet<String> {
    use crate::backend::docker::{dockerfile, image_tag, ImageCfg};
    let mut cfgs = Vec::new();
    let mut dirs = vec![env.home.join(".paddock-no-project")];
    dirs.extend(state::recent(env).into_iter().map(|r| r.project));
    for d in dirs {
        if let Ok(l) = crate::manifest::Manifest::load(env, &d) {
            let c = (l.merged.docker.image, l.merged.docker.packages);
            if !cfgs.contains(&c) {
                cfgs.push(c);
            }
        }
    }
    if cfgs.is_empty() {
        cfgs.push((None, vec![]));
    }
    let mut tags = BTreeSet::new();
    for (image, packages) in cfgs {
        for bits in 0..8u8 {
            let cfg = ImageCfg {
                image: image.clone(),
                packages: packages.clone(),
                gui: bits & 1 != 0,
                app: bits & 2 != 0,
                keyring: bits & 4 != 0,
            };
            if let Ok(df) = dockerfile(&cfg) {
                tags.insert(image_tag(&df));
            }
        }
    }
    tags
}

fn strip_registry(tag: &str) -> &str {
    tag.strip_prefix("localhost/")
        .or_else(|| tag.strip_prefix("docker.io/library/"))
        .unwrap_or(tag)
}

fn strip_sha(id: &str) -> &str {
    id.trim().strip_prefix("sha256:").unwrap_or(id.trim())
}

pub(super) fn scan_images(host: &Host, cli: &str, notes: &mut Vec<String>) -> Vec<Image> {
    let c = host.containers;
    let ids = match c.run(
        cli,
        &[
            "images",
            "-q",
            "--no-trunc",
            "--filter",
            "label=io.paddock.base=1",
        ],
    ) {
        Ok(s) => s,
        Err(_) => {
            notes.push(format!(
                "{cli}: cannot list images (is the daemon running?)"
            ));
            return vec![];
        }
    };
    let mut ids: Vec<&str> = ids
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return vec![];
    }
    let mut args = vec!["image", "inspect"];
    args.extend(&ids);
    let Ok(text) = c.run(cli, &args) else {
        notes.push(format!("{cli}: cannot inspect the paddock images"));
        return vec![];
    };
    let list: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
    // Every container, running or stopped, keeps its image.
    let mut used = BTreeSet::new();
    match c.run(cli, &["ps", "-a", "-q", "--no-trunc"]) {
        Ok(cids) => {
            let cids: Vec<&str> = cids
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            if !cids.is_empty() {
                let mut a = vec!["container", "inspect", "--format", "{{.Image}}"];
                a.extend(&cids);
                match c.run(cli, &a) {
                    Ok(out) => used.extend(out.lines().map(|l| strip_sha(l).to_string())),
                    Err(_) => {
                        notes.push(format!(
                            "{cli}: cannot inspect containers; keeping every image"
                        ));
                        return vec![];
                    }
                }
            }
        }
        Err(_) => {
            notes.push(format!(
                "{cli}: cannot list containers; keeping every image"
            ));
            return vec![];
        }
    }
    let unique = unique_sizes(host, cli);
    let mut out = Vec::new();
    for v in list {
        let id = strip_sha(v["Id"].as_str().unwrap_or_default()).to_string();
        let tags: Vec<String> = v["RepoTags"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        // An image built FROM paddock-base inherits the label. Only images
        // whose every tag is paddock's own are paddock's.
        if id.is_empty()
            || tags.is_empty()
            || !tags
                .iter()
                .all(|t| strip_registry(t).starts_with("paddock-base:"))
        {
            continue;
        }
        out.push(Image {
            cli: cli.to_string(),
            current: tags
                .iter()
                .any(|t| host.current_tags.contains(strip_registry(t))),
            in_use: used.contains(&id),
            bytes: v["Size"].as_u64().unwrap_or(0),
            // podman reports short ids.
            unique: unique.get(&id).copied().or_else(|| {
                unique
                    .iter()
                    .find(|(k, _)| k.len() >= 12 && id.starts_with(k.as_str()))
                    .map(|(_, v)| *v)
            }),
            created: v["Created"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .take(10)
                .collect(),
            id,
            tags,
        });
    }
    out
}
