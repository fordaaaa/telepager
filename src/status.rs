use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::client;
use crate::ipc;

/// What `telepager` should do about a daemon that may already be up.
enum Startup {
    /// One is already answering; its console is at this url.
    Attach(String),
    /// Nothing is running, so this process becomes the app.
    RunHere,
}

/// `telepager` and `telepager webui`: be the app, here, until this terminal
/// closes. Unconfigured is fine — the console serves the setup wizard.
/// `telepager start` is the one that outlives your shell.
pub fn open_console(config: Option<PathBuf>, open: bool) -> Result<()> {
    match startup_for(client::running_endpoint())? {
        // something already brought it up, and a second daemon would split the
        // bot token in two. just point at the one that's there.
        Startup::Attach(url) => {
            announce(&url, config.as_deref(), false, "`telepager stop` ends it.");
            if open {
                crate::web::open_in_browser(&url);
            }
            Ok(())
        }
        Startup::RunHere => run_in_foreground(config, open),
    }
}

/// `telepager start`: the app in the background, outliving this terminal,
/// until someone runs `telepager stop`.
pub fn start_background(config: Option<PathBuf>, open: bool) -> Result<()> {
    let already = client::running_endpoint().is_some();
    let endpoint = client::ensure_daemon(config.as_ref())?;
    let url = console_url(&endpoint)?;

    announce(&url, config.as_deref(), !already, "`telepager stop` ends it.");

    if open {
        crate::web::open_in_browser(&url);
    }
    Ok(())
}

/// A daemon that already answers is something to attach to, not an error —
/// that belongs to `telepager daemon`, which is asked to *be* the daemon.
fn startup_for(existing: Option<ipc::Endpoint>) -> Result<Startup> {
    match existing {
        Some(ep) => Ok(Startup::Attach(console_url(&ep)?)),
        None => Ok(Startup::RunHere),
    }
}

fn console_url(endpoint: &ipc::Endpoint) -> Result<String> {
    endpoint.ui_url().context(
        "the app is running but didn't publish a console address. \
         stop it with `telepager stop` and try again.",
    )
}

/// Be the app for as long as this terminal is open.
fn run_in_foreground(config: Option<PathBuf>, open: bool) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;

    runtime.block_on(async move {
        let core = crate::core::Core::new(config.clone());
        let mut serving = tokio::spawn(crate::daemon::serve_all(core, false));

        // serve_all publishes the console's address once it's listening
        let endpoint = tokio::select! {
            biased;
            done = &mut serving => return joined(done),
            ep = published_endpoint() => ep?,
        };

        let url = console_url(&endpoint)?;
        announce(&url, config.as_deref(), true, "ctrl-c ends it.");
        if open {
            crate::web::open_in_browser(&url);
        }

        let result = tokio::select! {
            done = &mut serving => joined(done),
            _ = shutdown_signal() => {
                println!();
                println!("telepager stopped.");
                Ok(())
            }
        };

        // a stale endpoint file sends clients at a port nothing is on
        ipc::clear_endpoint_if_ours();
        result
    })
}

/// Wait for the endpoint file to name *this* process — anyone else's would
/// mean attaching to them, and we only get here when nobody answered.
async fn published_endpoint() -> Result<ipc::Endpoint> {
    for _ in 0..300 {
        if let Some(ep) = ipc::read_endpoint() {
            if ep.pid == std::process::id() {
                return Ok(ep);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    anyhow::bail!("the console did not come up within 15s")
}

/// Ctrl-C, and the polite kill a shutdown or `telepager stop` sends.
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("could not listen for SIGTERM: {e}");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn joined(done: Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    match done {
        Ok(result) => result,
        Err(e) => Err(anyhow::anyhow!("the app stopped unexpectedly: {e}")),
    }
}

/// The few lines a person sees when the console is up.
fn announce(url: &str, config: Option<&Path>, started: bool, ending: &str) {
    if started {
        println!("telepager started.");
    }
    println!();
    println!("  console   {url}");

    match crate::config::load(config) {
        Ok(cfg) => println!("  telegram  connected, {} allowed user(s)", cfg.allowed_user_ids.len()),
        Err(_) => println!("  telegram  not connected yet — the page will walk you through it"),
    }

    println!();
    println!("  {ending}");
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
    // just the daemon. agents run in process groups of their own on purpose,
    // so they are not reachable from here and never were — signalling the
    // daemon's group would only have hit whatever shell started it.
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
            let model = master.model_or_default();
            match (master.is_usable(), master.provider.is_cli()) {
                // a cli backend with no model set uses whatever it's set to
                (true, true) if model.is_empty() => println!(
                    "  master    {} (its own login and model)",
                    master.provider.as_str()
                ),
                (true, _) => println!("  master    {} / {model}", master.provider.as_str()),
                (false, true) => println!(
                    "  master    {} isn't installed — install it or pick another in the console",
                    master.cli_command().unwrap_or_default()
                ),
                (false, false) => println!(
                    "  master    no api key — set {} or pick a provider in the console",
                    master.provider.default_key_env()
                ),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(ui: bool) -> ipc::Endpoint {
        ipc::Endpoint {
            port: 44013,
            token: "t".into(),
            pid: 4756,
            ui_port: ui.then_some(46375),
            ui_key: ui.then(|| "k".to_string()),
        }
    }

    #[test]
    fn an_app_that_is_already_up_is_opened_not_refused() {
        // `telepager` twice in two terminals is a normal thing to do. the
        // second one attaches; only `telepager daemon` treats this as an error.
        match startup_for(Some(endpoint(true))).unwrap() {
            Startup::Attach(url) => {
                assert!(url.contains("46375"), "{url}");
                assert!(url.contains("k=k"), "{url}");
            }
            Startup::RunHere => panic!("attached to nothing"),
        }
    }

    #[test]
    fn nothing_running_means_this_process_becomes_the_app() {
        assert!(matches!(startup_for(None).unwrap(), Startup::RunHere));
    }

    #[test]
    fn a_daemon_with_no_console_says_how_to_get_out_of_it() {
        let Err(err) = startup_for(Some(endpoint(false))) else {
            panic!("a console-less daemon looked fine");
        };
        assert!(format!("{err:#}").contains("telepager stop"));
    }
}
