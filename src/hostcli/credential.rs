//! `paddock-host credential`: the docker and aws credential channels.

use super::{send, usage, EXIT_NO_HOST, EXIT_USAGE};
use crate::broker::{Request, Status, PROTOCOL};
use std::collections::BTreeMap;

const CREDENTIAL_HELP: &str = "paddock-host credential: the docker and aws credential channels.

USAGE
  paddock-host credential docker get      (docker credential helper protocol)
  paddock-host credential aws NAME        (AWS credential_process output)

paddock sets these up itself when the user registered a docker or aws
credential: DOCKER_CONFIG names the helper docker-credential-paddock, and
AWS_CONFIG_FILE has a profile with this credential_process. You do not
need to call them by hand. The first use in a run asks the user.";

/// `credential docker ACTION` and `credential aws NAME`: the client side
/// of credentials/channels.rs. It prints only what docker or aws reads.
pub(crate) fn main(args: &[String]) -> i32 {
    use std::io::Read as _;
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let (kind, extra) = match words.as_slice() {
        ["docker", "get"] => {
            let mut server = String::new();
            if std::io::stdin()
                .take(4096)
                .read_to_string(&mut server)
                .is_err()
            {
                println!("paddock: cannot read the server address");
                return 1;
            }
            ("docker", ("server", server.trim().to_string()))
        }
        ["docker", "store" | "erase" | "list"] => {
            // docker reads the error from stdout.
            println!("paddock keeps the docker logins on the host: docker login, logout and a list of logins do not work in the sandbox");
            return 1;
        }
        ["aws", name] => ("aws", ("name", name.to_string())),
        ["-h" | "--help"] | [] => {
            println!("{CREDENTIAL_HELP}");
            return if args.is_empty() { EXIT_USAGE } else { 0 };
        }
        _ => return usage("credential needs docker get|store|erase|list or aws NAME"),
    };
    let mut a = BTreeMap::new();
    a.insert("kind".to_string(), serde_json::json!(kind));
    a.insert(extra.0.to_string(), serde_json::json!(extra.1));
    let req = Request {
        v: PROTOCOL,
        cap: crate::credentials::channels::CAPABILITY.into(),
        args: a,
        reason: String::new(),
    };
    match send(&req) {
        Ok(r) if r.status == Status::Done => {
            println!("{}", r.result);
            0
        }
        Ok(r) if kind == "docker" => {
            println!("{}", r.message);
            1
        }
        Ok(r) => {
            eprintln!("paddock-host: credential {kind}: {}", r.message);
            1
        }
        Err(e) if kind == "docker" => {
            println!("{e}");
            1
        }
        Err(e) => {
            eprintln!("paddock-host: {e}");
            EXIT_NO_HOST
        }
    }
}
