//! Read the output of one host command.

use anyhow::{bail, Result};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_OUTPUT: usize = 1024 * 1024;

/// The output of a command, read in a thread. A process that the command
/// left behind can keep the pipe open after the command ended, so the end
/// of the pipe is waited for only until a deadline.
pub(crate) struct Pipe {
    buf: Arc<Mutex<Vec<u8>>>,
    done: std::sync::mpsc::Receiver<()>,
}

/// How long the output may stay open after the command ended: enough to
/// read what it wrote.
pub(crate) const PIPE_GRACE: Duration = Duration::from_secs(2);

impl Pipe {
    /// Read `r` to its end, at most `cap` bytes.
    pub(crate) fn read(mut r: impl Read + Send + 'static, cap: usize) -> Self {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let (tx, done) = std::sync::mpsc::channel();
        let b = buf.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                let room = cap.saturating_sub(b.lock().unwrap().len()).min(chunk.len());
                match r.read(&mut chunk[..room]) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => b.lock().unwrap().extend_from_slice(&chunk[..n]),
                }
            }
            // It can hold a credential value.
            chunk.iter_mut().for_each(|c| *c = 0);
            let _ = tx.send(());
        });
        Self { buf, done }
    }

    /// What was read, once the pipe ended or `deadline` passed. False:
    /// the pipe is still open, and the output can be cut short.
    pub(crate) fn finish(self, deadline: Instant) -> (Vec<u8>, bool) {
        let wait = deadline.saturating_duration_since(Instant::now());
        let ended = self.done.recv_timeout(wait).is_ok();
        let out = std::mem::take(&mut *self.buf.lock().unwrap());
        (out, ended)
    }
}

pub fn run_capture(cmd: &mut Command, timeout: Duration) -> Result<(i32, String, String)> {
    run_capture_input(cmd, timeout, None)
}

/// Like `run_capture`, with `input` written to the command's stdin.
pub fn run_capture_input(
    cmd: &mut Command,
    timeout: Duration,
    input: Option<&[u8]>,
) -> Result<(i32, String, String)> {
    let mut child = cmd
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
        let data = data.to_vec();
        // A thread, so a command that does not read cannot block paddock.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let so = Pipe::read(child.stdout.take().unwrap(), MAX_OUTPUT);
    let se = Pipe::read(child.stderr.take().unwrap(), MAX_OUTPUT);
    let start = Instant::now();
    let code = loop {
        if let Some(s) = child.try_wait()? {
            break s.code().unwrap_or(-1);
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "the command did not finish in {} seconds and was stopped",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // A process left behind (an ssh ControlPersist master) can keep the
    // output open: take what came, within the time limit.
    let deadline = (start + timeout).min(Instant::now() + PIPE_GRACE);
    let out = String::from_utf8_lossy(&so.finish(deadline).0).into_owned();
    let err = String::from_utf8_lossy(&se.finish(deadline).0).into_owned();
    Ok((code, out, err))
}
