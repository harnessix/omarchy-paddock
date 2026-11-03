use anyhow::Result;
use paddock::env::HostEnv;
use paddock::state::{self, Running};

pub(crate) fn ps(env: &HostEnv, json: bool) -> Result<i32> {
    // Children follow their parent.
    let list = state::tree_order(Running::list(env));
    if json {
        let sessions = paddock::view::Tmux::find(env)
            .map(|t| t.sessions())
            .unwrap_or_default();
        let rows: Vec<serde_json::Value> = list
            .iter()
            .map(|r| {
                let mut v = serde_json::to_value(r).unwrap_or_default();
                if let (Some(o), Some((since, reason))) =
                    (v.as_object_mut(), paddock::freeze::frozen(env, &r.id))
                {
                    o.insert("frozen_since".into(), since.into());
                    o.insert("frozen_reason".into(), reason.into());
                }
                let timers = paddock::timer::read_summary(env, &r.id);
                if let (Some(o), false) = (v.as_object_mut(), timers.is_empty()) {
                    o.insert(
                        "timers".into(),
                        serde_json::to_value(timers).unwrap_or_default(),
                    );
                }
                if let Some(o) = v.as_object_mut() {
                    let (view, tmux) = paddock::view::available(env, &sessions, &r.id);
                    o.insert("view".into(), view.into());
                    o.insert("view_tmux".into(), tmux.into());
                    o.insert(
                        "detached".into(),
                        paddock::detach::is_detached(env, &r.id).into(),
                    );
                    if let Some(since) = paddock::detach::idle_since(env, &r.id) {
                        o.insert("idle_since".into(), since.into());
                    }
                    o.insert(
                        "activity".into(),
                        match paddock::view::activity(env, &r.id) {
                            Some(line) => serde_json::Value::String(line),
                            None => serde_json::Value::Null,
                        },
                    );
                }
                if let (Some(o), Some(t)) = (v.as_object_mut(), spec_task(env, &r.id)) {
                    o.insert("spec_task".into(), t.id.into());
                    o.insert("spec_feature".into(), t.feature.into());
                    o.insert("spec_state".into(), t.state.name().into());
                }
                if let (Some(o), true) = (v.as_object_mut(), paddock::shadow::is_shadow(env, &r.id))
                {
                    o.insert("profile".into(), paddock::shadow::DEFAULT.into());
                }
                if let Some(o) = v.as_object_mut() {
                    o.insert(
                        "boundary_count".into(),
                        paddock::boundary::count(env, &r.id).into(),
                    );
                    o.insert(
                        "compactions".into(),
                        paddock::compaction::count(env, &r.id).into(),
                    );
                }
                let spent = paddock::devices::ledger::spent_of(env, &r.id);
                if let (Some(o), false) = (v.as_object_mut(), spent.is_empty()) {
                    let rows: Vec<serde_json::Value> = spent
                        .iter()
                        .map(|s| {
                            serde_json::json!({
                                "device": s.device,
                                "spent": paddock::devices::ledger::money(s.spent),
                                "budget": paddock::devices::ledger::money(s.budget),
                            })
                        })
                        .collect();
                    o.insert("spent".into(), rows.into());
                }
                let tunnels = paddock::monitor::read_seen(env, &r.id);
                if let (Some(o), false) = (v.as_object_mut(), tunnels.is_empty()) {
                    o.insert("tunnels".into(), tunnels.into());
                }
                if let (Some(o), Some(t)) = (
                    v.as_object_mut(),
                    paddock::monitor::read_traffic(env, &r.id),
                ) {
                    o.insert(
                        "traffic".into(),
                        serde_json::json!({ "sent": t.sent, "received": t.received }),
                    );
                }
                let creds = paddock::monitor::read_credential_files(env, &r.id);
                if let (Some(o), false) = (v.as_object_mut(), creds.is_empty()) {
                    o.insert(
                        "credential_files".into(),
                        serde_json::to_value(creds).unwrap_or_default(),
                    );
                }
                // What the panel marks: not acknowledged, not muted.
                let marks = paddock::monitor::marks(env, &r.id, &r.project);
                if let (Some(o), false) = (v.as_object_mut(), marks.is_empty()) {
                    o.insert(
                        "monitor_marks".into(),
                        serde_json::to_value(marks).unwrap_or_default(),
                    );
                }
                v
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({ "api_version": "paddock.ps/v1", "sandboxes": rows, "ended": paddock::detach::recent_ended(env, state::now()) })
        );
    } else if list.is_empty() {
        println!("No running sandboxes.");
    } else {
        let now = state::now();
        let depth = |r: &Running| state::depth_in(&list, r);
        for r in &list {
            let id = format!("{}{}", "  ".repeat(depth(r)), r.id);
            let frozen = paddock::freeze::frozen(env, &r.id)
                .map(|(since, _)| format!("  frozen {}m", now.saturating_sub(since) / 60))
                .unwrap_or_default();
            let pasture = r
                .pasture
                .as_deref()
                .map(|p| format!("  pasture {p}"))
                .unwrap_or_default();
            let task = spec_task(env, &r.id)
                .map(|t| format!("  task {}", t.id))
                .unwrap_or_default();
            println!(
                "{:<28} {:<8} {:<7} {:<7} {:>5}m  {}{pasture}{task}{frozen}",
                id,
                r.harness,
                r.backend,
                r.gui,
                (now.saturating_sub(r.started)) / 60,
                r.project.display()
            );
        }
    }
    Ok(0)
}

pub(crate) fn freeze(env: &HostEnv, id: String, tree: bool, reason: String) -> Result<i32> {
    for f in paddock::freeze::freeze(env, &id, tree, &reason)? {
        println!("Froze {f}. Thaw it with: paddock thaw {f}");
    }
    Ok(0)
}

pub(crate) fn thaw(env: &HostEnv, id: String, tree: bool) -> Result<i32> {
    for f in paddock::freeze::thaw(env, &id, tree)? {
        println!("Thawed {f}.");
    }
    Ok(0)
}

pub(crate) fn wait(env: &HostEnv, id: String, timeout: Option<String>) -> Result<i32> {
    let t = match timeout {
        Some(t) => Some(std::time::Duration::from_secs(
            paddock::timer::parse_duration(&t)?,
        )),
        None => None,
    };
    // The tree is known only while the sandbox runs.
    let root = paddock::speckit::tree_root_of(env, &id);
    if let Some(t) = root
        .as_deref()
        .and_then(|r| paddock::speckit::record_of(env, r, &id))
    {
        eprintln!("paddock: {id} works on task {} of {}", t.id, t.feature);
    }
    let code = paddock::detach::wait(env, &id, t)?;
    if let Some(t) = root
        .as_deref()
        .and_then(|r| paddock::speckit::record_of(env, r, &id))
    {
        println!("task {} of {}: {}", t.id, t.feature, t.state.name());
    }
    Ok(code)
}

/// The Spec Kit task that a running sandbox holds.
fn spec_task(env: &HostEnv, id: &str) -> Option<paddock::speckit::board::Record> {
    let root = paddock::speckit::tree_root_of(env, id)?;
    paddock::speckit::record_of(env, &root, id).filter(|t| t.state.held())
}

pub(crate) fn view(env: &HostEnv, id: String, write: bool) -> Result<i32> {
    paddock::view::open(env, &id, write)?;
    Ok(0)
}

pub(crate) fn boundary(env: &HostEnv, id: String, json: bool, follow: bool) -> Result<i32> {
    paddock::boundary::cmd(env, &id, json, follow)
}
