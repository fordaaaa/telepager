use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::client;
use crate::ipc;

/// `telepager` with no arguments, and `telepager webui`: bring the app up if
/// it isn't already, then open the console. Configured or not — the console
/// serves the setup wizard, so there's nothing to do beforehand.
pub fn open_console(config: Option<PathBuf>, open: bool) -> Result<()> {
    let already = client::running_endpoint().is_some();
    let endpoint = client::ensure_daemon(config.as_ref())?;

    let url = endpoint.ui_url().context(
        "the app is running but didn't publish a console address. \
         stop it with `telepager stop` and try again.",
    )?;

    if !already {
        println!("telepager started.");
    }
    println!();
    println!("  console   {url}");

    match crate::config::load(config.as_deref()) {
        Ok(cfg) => println!("  telegram  connected, {} allowed user(s)", cfg.allowed_user_ids.len()),
        Err(_) => println!("  telegram  not connected yet — the page will walk you through it"),
    }

    println!();
    println!("  it keeps running in the background. `telepager stop` ends it.");

    if open {
        crate::web::open_in_browser(&url);
    }
    Ok(())
}

/// Stop the background app.
pub fn stop() -> Result<()> {
    let Some(endpoint) = client::running_endpoint() else {
        println!("telepager isn't running.");
        ipc::clear_endpoint();
        return Ok(());
    };

    terminate(endpoint.pid)?;
    ipc::clear_endpoint();
    println!("telepager stopped.");
    Ok(())
}

#[cfg(unix)]
fn terminate(pid: u32) -> Result<()> {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGTERM: i32 = 15;
    // the daemon is its own session leader, so this reaches it and its children
    let failed = unsafe { kill(pid as i32, SIGTERM) } != 0;
    if failed {
        anyhow::bail!("could not stop process {pid}");
    }
    Ok(())
}

#[cfg(windows)]
fn terminate(pid: u32) -> Result<()> {
    let status = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()
        .context("running taskkill")?;
    if !status.success() {
        anyhow::bail!("could not stop process {pid}");
    }
    Ok(())
}

// what a person sees when they type `telepager status`
pub fn print(config: Option<PathBuf>) {
    println!("telepager {}", env!("CARGO_PKG_VERSION"));
    println!();

    match crate::config::load(config.as_deref()) {
        Ok(cfg) => {
            println!("  config    ok, {} allowed user(s)", cfg.allowed_user_ids.len());
            let master = &cfg.master;
            if master.is_usable() {
                println!(
                    "  master    {} / {}",
                    master.provider.as_str(),
                    master.model_or_default()
                );
            } else {
                println!(
                    "  master    no api key — set {} or pick a provider in the console",
                    master.provider.default_key_env()
                );
            }
        }
        Err(e) => {
            println!("  config    not set up yet");
            println!("            {e}");
            println!();
            println!("  run `telepager` and it'll walk you through it in a browser.");
            println!("  or make a bot with @BotFather, get your id from @userinfobot,");
            println!("  and write bot_token and allowed_user_ids to");
            println!("  {}", ipc::config_hint());
            return;
        }
    }

    match client::running_endpoint() {
        Some(ep) => {
            println!("  app       running on 127.0.0.1:{}", ep.port);
            match ep.ui_url() {
                Some(url) => println!("  console   {url}"),
                None => println!("  console   not published"),
            }
        }
        None => println!("  app       not running — `telepager` starts it"),
    }

    println!();
    println!("  register it with your agent:");
    println!("    claude mcp add --scope user telepager -- telepager mcp");
    println!();
    println!("  then ask it to message you. `telepager --help` for the rest.");
}

// an mcp client pipes json at us, a person doesn't
pub fn started_by_a_person() -> bool {
    std::io::stdin().is_terminal()
}
