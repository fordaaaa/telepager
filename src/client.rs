use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::ipc::{self, Request, Response};

pub struct Client {
    write: TcpStream,
    read: BufReader<TcpStream>,
}

impl Client {
    // connect to a running daemon, or start one and wait for it to come up
    pub fn connect(config: Option<&PathBuf>) -> Result<Self> {
        if let Some(c) = Self::try_connect() {
            return Ok(c);
        }

        spawn_daemon(config)?;

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            if let Some(c) = Self::try_connect() {
                return Ok(c);
            }
        }
        bail!("the daemon did not come up within 10s")
    }

    fn try_connect() -> Option<Self> {
        let ep = ipc::read_endpoint()?;
        let stream = TcpStream::connect(("127.0.0.1", ep.port)).ok()?;
        let read = BufReader::new(stream.try_clone().ok()?);
        let mut c = Client { write: stream, read };
        c.send(&Request::Hello {
            token: ep.token,
            label: ipc::session_label(),
        })
        .ok()?;
        Some(c)
    }

    fn send(&mut self, req: &Request) -> Result<()> {
        let line = serde_json::to_string(req)?;
        self.write.write_all(line.as_bytes())?;
        self.write.write_all(b"\n")?;
        self.write.flush()?;
        Ok(())
    }

    // hello gets no reply, everything else does
    pub fn call(&mut self, req: &Request) -> Result<String> {
        self.send(req)?;
        let mut line = String::new();
        let n = self.read.read_line(&mut line).context("reading the reply")?;
        if n == 0 {
            bail!("the daemon closed the connection");
        }
        let resp: Response = serde_json::from_str(line.trim()).context("bad reply")?;
        Ok(resp.result)
    }
}

fn spawn_daemon(config: Option<&PathBuf>) -> Result<()> {
    let exe = std::env::current_exe().context("finding our own binary")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon");
    if let Some(c) = config {
        cmd.arg("--config").arg(c);
    }
    // the daemon outlives this process, and its output isn't ours to carry
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                // leave our process group so a client exiting doesn't kill it
                libc_setsid();
                Ok(())
            });
        }
    }

    cmd.spawn().context("starting the daemon")?;
    Ok(())
}

#[cfg(unix)]
fn libc_setsid() {
    unsafe extern "C" {
        fn setsid() -> i32;
    }
    unsafe {
        setsid();
    }
}
