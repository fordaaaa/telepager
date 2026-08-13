mod client;
mod config;
mod daemon;
mod ipc;
mod mcp;
mod telegram;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
telepager — page a telegram user from an mcp client

usage: telepager [--config PATH]
       telepager daemon [--config PATH]

  daemon          run the background process that owns the telegram
                  connection. started automatically when needed.

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
        Action::Run(c) => c,
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
    Run(Option<PathBuf>),
    Daemon(Option<PathBuf>),
    Exit,
}

fn parse_args() -> Result<Action, String> {
    let mut config = None;
    let mut daemon = false;
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
            "daemon" => daemon = true,
            "--config" => {
                let path = args.next().ok_or("--config needs a path")?;
                config = Some(PathBuf::from(path));
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }

    if daemon {
        return Ok(Action::Daemon(config));
    }
    Ok(Action::Run(config))
}
