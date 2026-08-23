mod agents;
mod cli_master;
mod client;
mod config;
mod core;
mod daemon;
mod ipc;
mod llm;
mod master;
mod master_mcp;
mod mcp;
mod screen;
mod session;
mod status;
mod telegram;
mod web;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
telepager — run and watch coding agents from your phone

usage: telepager [--no-open] [--config PATH]
       telepager webui [--no-open] [--config PATH]
       telepager start [--no-open] [--config PATH]
       telepager setup [--port N] [--no-open] [--config PATH]
       telepager status [--config PATH]
       telepager mcp [--config PATH]
       telepager master-mcp
       telepager daemon [--no-open] [--config PATH]
       telepager stop

  (no command)    run the app here and open the console in your browser. it
                  lives in this terminal — ctrl-c ends it. runs setup first if
                  it isn't configured yet.
  webui           the same thing, said out loud. attaches to the app if one is
                  already up, instead of running one.
  start           run the app in the background instead, so it outlives this
                  terminal. `telepager stop` ends it.
  setup           connect a telegram bot: paste a token, pick up your user id.
  status          show whether it's set up and running.
  mcp             the stdio server an mcp client starts. this is what you
                  register with claude code, cursor and friends.
  master-mcp      the tools a cli-backed master agent reaches telepager
                  through. started by telepager itself, not by you.
  daemon          run the app in the foreground without opening a browser.
  stop            stop the background app.

  --config PATH   config file to use, instead of the default lookup
                  (./telepager.config.json, then telepager/config.json
                  in your user config dir)
  --port N        port for the console (default: any free one)
  --no-open       don't try to launch a browser
  -h, --help      show this
  -V, --version   show the version
";

fn main() -> ExitCode {
    restore_sigpipe();

    let action = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            eprint!("\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    let config = match action {
        Action::Status(c) => {
            status::print(c);
            return ExitCode::SUCCESS;
        }
        Action::Stop => return finish(status::stop()),
        Action::App { config, open } => {
            init_logging();
            return finish(status::open_console(config, open));
        }
        Action::Start { config, open } => {
            init_logging();
            return finish(status::start_background(config, open));
        }
        Action::Setup { config, port, open } => {
            init_logging();
            return finish(web::run_standalone(config, port, open));
        }
        Action::MasterMcp => {
            init_logging();
            return finish(master_mcp::run());
        }
        Action::Mcp(c) => c,
        Action::Daemon { config, open } => {
            init_logging();
            return match daemon::run(config, open) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e:#}");
                    ipc::clear_endpoint_if_ours();
                    ExitCode::FAILURE
                }
            };
        }
        Action::Exit => return ExitCode::SUCCESS,
    };

    init_logging();

    match mcp::run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn finish(result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Rust ignores SIGPIPE, so `telepager status | head` panics on the first
/// write after head exits. Put the default handler back and we exit quietly
/// like every other command line tool.
#[cfg(unix)]
fn restore_sigpipe() {
    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe() {}

// stdout is the mcp transport so logs have to go to stderr
fn init_logging() {
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "info");
    }
    let _ = env_logger::try_init();
}

enum Action {
    /// Be the app for as long as this terminal is open, and open the console.
    App { config: Option<PathBuf>, open: bool },
    /// Put the app in the background and open the console.
    Start { config: Option<PathBuf>, open: bool },
    Setup { config: Option<PathBuf>, port: u16, open: bool },
    Status(Option<PathBuf>),
    Mcp(Option<PathBuf>),
    /// The MCP bridge a CLI-backed master agent calls telepager through.
    MasterMcp,
    Daemon { config: Option<PathBuf>, open: bool },
    Stop,
    Exit,
}

fn parse_args() -> Result<Action, String> {
    parse(std::env::args().skip(1), status::started_by_a_person())
}

/// Split out from [`parse_args`] so the grammar is testable.
fn parse(args: impl Iterator<Item = String>, by_a_person: bool) -> Result<Action, String> {
    let mut config = None;
    let mut mode = None;
    let mut port = 0u16;
    let mut open = true;
    let mut args = args;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(Action::Exit);
            }
            "-V" | "--version" => {
                println!("telepager {}", env!("CARGO_PKG_VERSION"));
                return Ok(Action::Exit);
            }
            "mcp" | "master-mcp" | "daemon" | "status" | "setup" | "webui" | "ui" | "start"
            | "stop" => mode = Some(arg),
            "--port" => {
                let value = args.next().ok_or("--port needs a number")?;
                port = value
                    .parse()
                    .map_err(|_| format!("--port: '{value}' is not a port number"))?;
            }
            "--no-open" => open = false,
            "--config" => {
                let path = args.next().ok_or("--config needs a path")?;
                config = Some(PathBuf::from(path));
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }

    match mode.as_deref() {
        Some("setup") => Ok(Action::Setup { config, port, open }),
        Some("webui") | Some("ui") => Ok(Action::App { config, open }),
        Some("start") => Ok(Action::Start { config, open }),
        Some("daemon") => Ok(Action::Daemon { config, open: false }),
        Some("mcp") => Ok(Action::Mcp(config)),
        Some("master-mcp") => Ok(Action::MasterMcp),
        Some("status") => Ok(Action::Status(config)),
        Some("stop") => Ok(Action::Stop),
        // a person typing `telepager` wants the app. a client piping json at
        // us is an mcp registration, and those keep working.
        _ if by_a_person => Ok(Action::App { config, open }),
        _ => Ok(Action::Mcp(config)),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Action};

    fn args(list: &[&str]) -> impl Iterator<Item = String> {
        list.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn the_usage_documents_every_command() {
        for command in ["webui", "setup", "status", "mcp", "master-mcp", "daemon", "start", "stop"] {
            assert!(
                super::USAGE.contains(command),
                "usage doesn't mention {command}"
            );
        }
    }

    #[test]
    fn a_bare_telepager_from_a_person_runs_the_app_here() {
        assert!(matches!(
            parse(args(&[]), true).unwrap(),
            Action::App { open: true, .. }
        ));
    }

    #[test]
    fn json_on_stdin_is_still_an_mcp_registration() {
        assert!(matches!(parse(args(&[]), false).unwrap(), Action::Mcp(None)));
    }

    #[test]
    fn start_is_the_background_one() {
        // `telepager` is foreground now, so the background one is asked for by name
        assert!(matches!(
            parse(args(&["start"]), true).unwrap(),
            Action::Start { open: true, .. }
        ));
        assert!(matches!(
            parse(args(&["start", "--no-open"]), true).unwrap(),
            Action::Start { open: false, .. }
        ));
    }

    #[test]
    fn webui_takes_the_foreground_path_too() {
        for name in ["webui", "ui"] {
            assert!(
                matches!(parse(args(&[name]), true).unwrap(), Action::App { .. }),
                "{name} isn't the app"
            );
        }
    }
}
