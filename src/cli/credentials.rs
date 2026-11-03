use anyhow::Result;
use clap::Subcommand;
use paddock::env::HostEnv;

#[derive(Subcommand)]
pub(crate) enum CredentialsCmd {
    /// List the registered credentials.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Stop a running sandbox from using a credential through the gateway or
    /// the ssh agent channel.
    /// Without NAME: every one. Its next request fails; it keeps running.
    Revoke { id: String, name: Option<String> },
}

pub(crate) fn run(env: &HostEnv, cmd: CredentialsCmd) -> Result<i32> {
    match cmd {
        CredentialsCmd::Revoke { id, name } => {
            let names = paddock::credentials::revoke(env, &id, name.as_deref())?;
            println!(
                "Revoked for {id}: {}. The gateway refuses their next request with 403, the ssh agent channel signs no more, and their secret files are gone.",
                names.join(", ")
            );
            Ok(0)
        }
        CredentialsCmd::List { json } => {
            let rows = paddock::credentials::rows(env)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "api_version": "paddock.credentials/v1", "credentials": rows,
                    }))?
                );
                return Ok(0);
            }
            if rows.is_empty() {
                println!(
                    "No credentials are registered. Add [credentials.NAME] to {}.",
                    env.paddock_config().join("config.toml").display()
                );
            }
            for r in &rows {
                println!(
                    "{:<24} {:<9} {:<28} {}{}",
                    r.name,
                    r.via,
                    r.source,
                    if !r.available {
                        "not available yet".to_string()
                    } else if !r.ready {
                        "source missing".to_string()
                    } else {
                        "ready".to_string()
                    },
                    match r.last_delivered {
                        Some(t) => format!(
                            ", delivered {} times, last {}",
                            r.delivered,
                            paddock::storage::date(t)
                        ),
                        None => String::new(),
                    },
                );
                if r.used > 0 {
                    if r.via == "agent" {
                        println!("{:<24} {} ssh signatures allowed", "", r.used);
                    } else if r.deliver_as == Some("file") {
                        println!("{:<24} {} reads of the secret file", "", r.used);
                    } else {
                        println!("{:<24} {} requests through the gateway", "", r.used);
                    }
                }
            }
            Ok(0)
        }
    }
}
