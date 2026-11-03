use anyhow::Result;
use paddock::env::HostEnv;
use paddock::state;

pub(crate) fn run(env: &HostEnv, agents: Vec<String>, refresh: bool, json: bool) -> Result<i32> {
    let agents = if agents.is_empty() {
        paddock::forage::known_agents(env)
    } else {
        agents
    };
    if refresh {
        paddock::forage::refresh(env, &agents)?;
    }
    for a in &agents {
        paddock::forage::record(env, a)?;
    }
    let windows: Vec<paddock::forage::Window> = agents
        .iter()
        .flat_map(|a| paddock::forage::windows(env, a))
        .filter(|w| w.used > 0.0 || w.resets.is_some())
        .collect();
    let v = serde_json::json!({"api_version": "paddock.forage/v1", "windows": windows});
    if json {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print!("{}", paddock::hostcli::forage::render(&v, state::now()));
    }
    Ok(0)
}
