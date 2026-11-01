//! `paddock-host pasture`: share local ports with the other members of the
//! pasture. See `crate::pasture::ports` for how the traffic flows.

use crate::kv::Status;
use crate::pasture::ports::{self, Shared};
use std::time::{Duration, Instant};

pub const HELP: &str = r#"paddock-host pasture: share local ports with the other members of your pasture.

USAGE
  paddock-host pasture share PORT [--json]
  paddock-host pasture unshare PORT [--json]
  paddock-host pasture members [--json]

Each member of a pasture is its own sandbox with its own network. share PORT
puts a TCP port of this sandbox "on the ground": a server that listens on
127.0.0.1:PORT here is then reachable on 127.0.0.1:PORT in every other
member. Nothing else leaves this sandbox: only the ports that you name.

  share PORT     Share 127.0.0.1:PORT of this sandbox. Start the server
                 before or after. When another member shares PORT already,
                 share exits with 9 and names that member.
  unshare PORT   Stop sharing PORT. The share also ends when this sandbox
                 ends (within 30 seconds).
  members        The members of the pasture, their harness and the ports
                 that each one shares.

When a member already has its own server on PORT, it skips that port and
members shows why. Limits: TCP only, ports 1024 to 65535, at most 16 shared
ports per pasture.

EXIT CODES
  0 done   1 not shared by this sandbox   2 wrong usage   6 no host
  connection or no pasture   9 another member shares the port   10 the
  pasture shares 16 ports already

EXAMPLES
  npm run dev -- --port 3000 &
  paddock-host pasture share 3000
  paddock-host pasture members
  curl http://127.0.0.1:3000/      (in another member)
"#;

fn usage(msg: &str) -> i32 {
    eprintln!(
        "paddock-host pasture: {msg}\nRun `paddock-host pasture --help` for the full reference."
    );
    super::EXIT_USAGE
}

fn no_host(e: &str) -> i32 {
    eprintln!("paddock-host pasture: {e}");
    super::EXIT_NO_HOST
}

pub fn main(args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");
    let pos: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--json")
        .collect();
    match pos.as_slice() {
        [] => {
            print!("{HELP}");
            super::EXIT_USAGE
        }
        [h, ..] if matches!(*h, "-h" | "--help" | "help") => {
            print!("{HELP}");
            0
        }
        _ if pos.iter().any(|a| *a == "-h" || *a == "--help") => {
            print!("{HELP}");
            0
        }
        ["serve"] => ports::serve(),
        ["share", p] => match ports::check_port(p) {
            Ok(p) => share(p, json),
            Err(e) => usage(&e),
        },
        ["unshare", p] => match ports::check_port(p) {
            Ok(p) => unshare(p, json),
            Err(e) => usage(&e),
        },
        ["members"] => members(json),
        [cmd, ..] if matches!(*cmd, "share" | "unshare") => usage(&format!("{cmd} needs one PORT")),
        [cmd, ..] => usage(&format!("unknown pasture command '{cmd}'")),
    }
}

/// Check that this sandbox is in a pasture. Returns its id.
fn member() -> Result<String, i32> {
    match ports::whoami() {
        Ok((me, _, Some(_))) => Ok(me),
        Ok(_) => Err(no_host(
            "this sandbox is in no pasture: the user starts members with --pasture NAME",
        )),
        Err(e) => Err(no_host(&format!(
            "{e}\nThis command works only inside a paddock sandbox."
        ))),
    }
}

/// Start the helper of this member when none runs, for example in a
/// backend that did not start it with the sandbox.
fn ensure_helper(me: &str) {
    let ground = ports::ground();
    if ports::make_dir(&ground).is_err() {
        return;
    }
    let Some(lock) = ports::try_lock(&ground, me) else {
        return;
    };
    drop(lock);
    let exe = std::env::current_exe().unwrap_or_else(|_| "paddock".into());
    let _ = std::process::Command::new(exe)
        .args(["host", "pasture", "serve"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Wait until the socket of `port` exists (`present`) or not.
fn wait_socket(port: u16, present: bool) -> bool {
    let path = ports::socket_path(&ports::ground(), port);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if path.exists() == present {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn listens(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(300),
    )
    .is_ok()
}

fn share(port: u16, json: bool) -> i32 {
    let me = match member() {
        Ok(m) => m,
        Err(code) => return code,
    };
    let resp = match ports::claim(port) {
        Ok(r) => r,
        Err(e) => return no_host(&e),
    };
    if resp.status != Status::Done {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&resp).unwrap_or_default()
            );
        } else if resp.status == Status::Conflict {
            let owner = resp.result["owner"].as_str().unwrap_or("another member");
            let harness = ports::shared()
                .unwrap_or_default()
                .into_iter()
                .find(|s| s.port == port)
                .map(|s| format!("{} ", s.harness))
                .unwrap_or_default();
            eprintln!(
                "paddock-host pasture: port {port} is shared by {harness}(sandbox {owner}) already"
            );
        } else {
            eprintln!("paddock-host pasture: share {port}: {}", resp.message);
        }
        return resp.status.exit_code();
    }
    ensure_helper(&me);
    let ready = wait_socket(port, true);
    let up = listens(port);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "port": port, "owner": me, "ready": ready, "listening": up,
            }))
            .unwrap_or_default()
        );
        return 0;
    }
    println!("port {port} is shared with the pasture");
    if !ready {
        eprintln!("paddock-host pasture: the relay of this sandbox did not start yet; the other members reach the port once it runs");
    }
    if !up {
        eprintln!("paddock-host pasture: nothing listens on 127.0.0.1:{port} here yet");
    }
    0
}

fn unshare(port: u16, json: bool) -> i32 {
    if let Err(code) = member() {
        return code;
    }
    let resp = match ports::release(port) {
        Ok(r) => r,
        Err(e) => return no_host(&e),
    };
    if resp.status == Status::Done {
        wait_socket(port, false);
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&resp).unwrap_or_default()
        );
        return resp.status.exit_code();
    }
    match resp.status {
        Status::Done => {
            println!("port {port} is no longer shared");
            0
        }
        Status::NotFound => {
            eprintln!("paddock-host pasture: port {port} is not shared");
            1
        }
        s => {
            let owner = resp.result["owner"].as_str().unwrap_or("another member");
            eprintln!(
                "paddock-host pasture: port {port} belongs to {owner}; only it can unshare it"
            );
            s.exit_code()
        }
    }
}

fn members(json: bool) -> i32 {
    let me = match member() {
        Ok(m) => m,
        Err(code) => return code,
    };
    let shared = match ports::shared() {
        Ok(s) => s,
        Err(e) => return no_host(&e),
    };
    let records = match member_records() {
        Ok(r) => r,
        Err(e) => return no_host(&e),
    };
    // Every member with a record, plus owners of ports without one.
    let mut rows: Vec<(String, String, serde_json::Value)> = records;
    for s in &shared {
        if !rows.iter().any(|(id, _, _)| *id == s.sandbox) {
            rows.push((
                s.sandbox.clone(),
                s.harness.clone(),
                serde_json::Value::Null,
            ));
        }
    }
    let ports_of = |id: &str| -> Vec<u16> {
        shared
            .iter()
            .filter(|s: &&Shared| s.sandbox == id)
            .map(|s| s.port)
            .collect()
    };
    if json {
        let list: Vec<serde_json::Value> = rows
            .iter()
            .map(|(id, harness, skipped)| {
                serde_json::json!({
                    "sandbox": id, "harness": harness, "you": *id == me,
                    "ports": ports_of(id), "skipped": skipped,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "members": list }))
                .unwrap_or_default()
        );
        return 0;
    }
    for (id, harness, skipped) in &rows {
        let p = ports_of(id);
        let p = if p.is_empty() {
            "-".to_string()
        } else {
            p.iter().map(u16::to_string).collect::<Vec<_>>().join(", ")
        };
        let you = if *id == me { "  (you)" } else { "" };
        println!("{id:<28} {harness:<10} ports: {p}{you}");
        for (port, why) in skipped.as_object().into_iter().flatten() {
            println!("  skips {port}: {}", why.as_str().unwrap_or(""));
        }
    }
    0
}

/// The member records that the helpers keep: id, harness, skipped ports.
fn member_records() -> Result<Vec<(String, String, serde_json::Value)>, String> {
    let req = |op: &str| crate::kv::Request {
        v: crate::kv::PROTOCOL,
        op: op.into(),
        scope: Some(crate::kv::Scope::Pasture),
        ..Default::default()
    };
    let mut l = req("list");
    l.prefix = Some(ports::MEMBER_PREFIX.into());
    let r = super::kv::send(&l)?;
    if r.status != Status::Done {
        return Err(r.message);
    }
    let keys: Vec<String> = r.result["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["key"].as_str().map(str::to_string))
        .collect();
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let mut g = req("mget");
    g.keys = Some(keys);
    let r = super::kv::send(&g)?;
    if r.status != Status::Done {
        return Err(r.message);
    }
    let mut out = Vec::new();
    for e in r.result["entries"].as_array().into_iter().flatten() {
        // The writer, not the key: a member cannot write the record of
        // another one.
        let Some(id) = e["writer"].as_str().or_else(|| {
            e["key"]
                .as_str()
                .and_then(|k| k.strip_prefix(ports::MEMBER_PREFIX))
        }) else {
            continue;
        };
        let v: serde_json::Value =
            serde_json::from_str(e["value"].as_str().unwrap_or("")).unwrap_or_default();
        let harness = v["harness"].as_str().unwrap_or("").to_string();
        let skipped = match &v["skipped"] {
            serde_json::Value::Object(m) if !m.is_empty() => v["skipped"].clone(),
            _ => serde_json::Value::Null,
        };
        out.push((id.to_string(), harness, skipped));
    }
    Ok(out)
}
