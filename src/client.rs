use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::ipc::{self, Request, Response};

pub struct Client {
    write: TcpStream,
    read: BufReader<TcpStream>,
    // kept so a reconnect can start a daemon the same way we first did
    config: Option<PathBuf>,
}

impl Client {
    // connect to a running daemon, or start one and wait for it to come up
    pub fn connect(config: Option<&PathBuf>) -> Result<Self> {
        if let Some(c) = Self::try_connect(config) {
            return Ok(c);
        }

        spawn_daemon(config)?;

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            if let Some(c) = Self::try_connect(config) {
                return Ok(c);
            }
        }
        bail!("the daemon did not come up within 10s")
    }

    fn try_connect(config: Option<&PathBuf>) -> Option<Self> {
        let ep = ipc::read_endpoint()?;
        let stream = TcpStream::connect(("127.0.0.1", ep.port)).ok()?;
        let read = BufReader::new(stream.try_clone().ok()?);
        let mut c = Client {
            write: stream,
            read,
            config: config.cloned(),
        };
        c.send(&Request::Hello {
            token: ep.token,
            label: ipc::session_label(),
            cwd: std::env::current_dir().ok(),
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

    // the daemon can restart under us — an upgrade, a kill, a laptop waking up.
    // one silent reconnect beats making the user restart their editor.
    pub fn call(&mut self, req: &Request) -> Result<String> {
        match self.call_once(req) {
            Ok(result) => Ok(result),
            Err(_) => {
                self.reconnect()?;
                self.call_once(req)
            }
        }
    }

    fn call_once(&mut self, req: &Request) -> Result<String> {
        self.send(req)?;
        let mut line = String::new();
        let n = self.read.read_line(&mut line).context("reading the reply")?;
        if n == 0 {
            bail!("the daemon closed the connection");
        }
        let resp: Response = serde_json::from_str(line.trim()).context("bad reply")?;
        Ok(resp.result)
    }

    fn reconnect(&mut self) -> Result<()> {
        let fresh = Self::connect(self.config.as_ref()).context("reconnecting to the daemon")?;
        *self = fresh;
        Ok(())
    }
}

/// Make sure a daemon is up, starting one if it isn't, and say where it is.
/// Whichever command you run first brings the app up; the rest attach.
pub fn ensure_daemon(config: Option<&PathBuf>) -> Result<ipc::Endpoint> {
    if let Some(ep) = running_endpoint() {
        return Ok(ep);
    }

    spawn_daemon(config)?;

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(150));
        if let Some(ep) = running_endpoint() {
            return Ok(ep);
        }
    }
    bail!("the telepager daemon did not come up within 15s")
}

/// The endpoint file only means something if the daemon it describes answers.
pub fn running_endpoint() -> Option<ipc::Endpoint> {
    let ep = ipc::read_endpoint()?;
    let addr = format!("127.0.0.1:{}", ep.port).parse().ok()?;
    let sock = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    let _ = sock.shutdown(std::net::Shutdown::Both);
    Some(ep)
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
