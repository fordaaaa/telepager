use std::io::IsTerminal;
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use crate::ipc;

// what a person sees when they type `telepager` and expect something to happen
pub fn print(config: Option<PathBuf>) {
    println!("telepager {}", env!("CARGO_PKG_VERSION"));
    println!();

    match crate::config::load(config.as_deref()) {
        Ok(cfg) => {
            println!("  config    ok, {} allowed user(s)", cfg.allowed_user_ids.len());
        }
        Err(e) => {
            println!("  config    not set up yet");
            println!("            {e}");
            println!();
            println!("  run `telepager setup` and it'll walk you through it in a");
            println!("  browser. or make a bot with @BotFather, get your id from");
            println!("  @userinfobot, and write bot_token and allowed_user_ids to");
            println!("  {}", ipc::config_hint());
            return;
        }
    }

    match daemon_state() {
        Some(port) => println!("  daemon    running on 127.0.0.1:{port}"),
        None => println!("  daemon    not running (starts on its own when a client needs it)"),
    }

    println!();
    println!("  register it with your agent:");
    println!("    claude mcp add --scope user telepager -- telepager mcp");
    println!();
    println!("  then ask it to message you. `telepager --help` for the rest.");
}

// a daemon is only really up if something answers on its port
fn daemon_state() -> Option<u16> {
    let ep = ipc::read_endpoint()?;
    let addr = format!("127.0.0.1:{}", ep.port).parse().ok()?;
    let sock = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    let _ = sock.shutdown(Shutdown::Both);
    Some(ep.port)
}

// an mcp client pipes json at us, a person doesn't
pub fn started_by_a_person() -> bool {
    std::io::stdin().is_terminal()
}
