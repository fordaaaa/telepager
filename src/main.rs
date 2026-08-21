mod agents;
mod client;
mod config;
mod core;
mod daemon;
mod ipc;
mod llm;
mod master;
mod mcp;
mod session;
mod status;
mod telegram;
mod web;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
telepager — run and watch coding agents from your phone

usage: telepager [--config PATH]
       telepager webui [--no-open] [--config PATH]
       telepager setup [--port N] [--no-open] [--config PATH]
       telepager status [--config PATH]
       telepager mcp [--config PATH]
       telepager daemon [--no-open] [--config PATH]
       telepager stop

  (no command)    start the app and open the console in your browser. runs
                  setup first if it isn't configured yet.
  webui           the same thing, said out loud. opens the console, starting
                  the app first if it isn't already up.
  setup           connect a telegram bot: paste a token, pick up your user id.
  status          show whether it's set up and running.
  mcp             the stdio server an mcp client starts. this is what you
                  register with claude code, cursor and friends.
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
        Action::Setup { config, port, open } => {
            init_logging();
            return finish(web::run_standalone(config, port, open));
        }
        Action::Mcp(c) => c,
        Action::Daemon { config, open } => {
            init_logging();
            return match daemon::run(config, open) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e:#}");
                    ipc::clear_endpoint();
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

// stdout is the mcp transport so logs have to go to stderr
fn init_logging() {
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "info");
    }
    let _ = env_logger::try_init();
}

enum Action {
    /// Bring the app up and open the console.
    App { config: Option<PathBuf>, open: bool },
    Setup { config: Option<PathBuf>, port: u16, open: bool },
    Status(Option<PathBuf>),
    Mcp(Option<PathBuf>),
    Daemon { config: Option<PathBuf>, open: bool },
    Stop,
    Exit,
}

fn parse_args() -> Result<Action, String> {
    let mut config = None;
    let mut mode = None;
    let mut port = 0u16;
    let mut open = true;
    let mut args = std::env::args().skip(1);

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
            "mcp" | "daemon" | "status" | "setup" | "webui" | "ui" | "stop" => mode = Some(arg),
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
        Some("daemon") => Ok(Action::Daemon { config, open: false }),
        Some("mcp") => Ok(Action::Mcp(config)),
        Some("status") => Ok(Action::Status(config)),
        Some("stop") => Ok(Action::Stop),
        // a person typing `telepager` wants the app. a client piping json at
        // us is an mcp registration, and those keep working.
        _ if status::started_by_a_person() => Ok(Action::App { config, open }),
        _ => Ok(Action::Mcp(config)),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_usage_documents_every_command() {
        for command in ["webui", "setup", "status", "mcp", "daemon", "stop"] {
            assert!(
                super::USAGE.contains(command),
                "usage doesn't mention {command}"
            );
        }
    }
}
