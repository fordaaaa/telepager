mod client;
mod config;
mod daemon;
mod ipc;
mod mcp;
mod telegram;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
telepager — page yourself on telegram from a coding agent

usage: telepager mcp [--config PATH]
       telepager daemon [--config PATH]

  mcp             the stdio server an mcp client starts. this is what you
                  register with claude code, cursor and friends.
  daemon          the background process that owns the telegram connection.
                  started automatically when a client needs it.

  --config PATH   config file to use, instead of the default lookup
                  (./telepager.config.json, then telepager/config.json
                  in your user config dir)
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
        Action::Mcp(c) => c,
        Action::Daemon(c) => {
            init_logging();
            return match daemon::run(c) {
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

// stdout is the mcp transport so logs have to go to stderr
fn init_logging() {
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "info");
    }
    let _ = env_logger::try_init();
}

enum Action {
    Mcp(Option<PathBuf>),
    Daemon(Option<PathBuf>),
    Exit,
}

fn parse_args() -> Result<Action, String> {
    let mut config = None;
    let mut mode = None;
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
            "mcp" | "daemon" => mode = Some(arg),
            "--config" => {
                let path = args.next().ok_or("--config needs a path")?;
                config = Some(PathBuf::from(path));
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }

    match mode.as_deref() {
        Some("daemon") => Ok(Action::Daemon(config)),
        Some("mcp") => Ok(Action::Mcp(config)),
        // registrations from before the subcommand existed
        _ => {
            eprintln!("telepager: no subcommand given, assuming 'mcp'. update your");
            eprintln!("registration to `telepager mcp` — the bare form goes away later.");
            Ok(Action::Mcp(config))
        }
    }
}
