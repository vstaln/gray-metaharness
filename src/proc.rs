//! Small synchronous process helper with a hard timeout. The sidecar is
//! blocking std I/O (no async), so `policy.py` and `check.sh` runs can't
//! lean on tokio — poll `try_wait`, kill on expiry. Reader threads drain
//! the pipes so a chatty child can't deadlock on a full buffer.

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub struct ProcOut {
    pub stdout: Vec<u8>,
    pub timed_out: bool,
}

impl ProcOut {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// Run `cmd` to completion or `timeout`, feeding `stdin` if given.
pub fn run(mut cmd: Command, stdin: Option<Vec<u8>>, timeout: Duration) -> io::Result<ProcOut> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    if let Some(bytes) = stdin {
        let mut pipe = child.stdin.take().expect("piped stdin");
        thread::spawn(move || {
            let _ = pipe.write_all(&bytes);
            // drop → EOF so the child sees a closed stdin
        });
    }

    fn drain<T: Read + Send + 'static>(r: T) -> thread::JoinHandle<Vec<u8>> {
        thread::spawn(move || {
            let mut v = Vec::new();
            let mut r = r;
            let _ = r.read_to_end(&mut v);
            v
        })
    }
    let out_h = drain(child.stdout.take().expect("piped stdout"));
    let err_h = drain(child.stderr.take().expect("piped stderr"));

    let start = Instant::now();
    let mut timed_out = false;
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None => {
                if start.elapsed() > timeout {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(15));
            }
        }
    }
    let stdout = out_h.join().unwrap_or_default();
    let _ = err_h.join();
    Ok(ProcOut { stdout, timed_out })
}
