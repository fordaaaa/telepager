mod config;
mod ipc;
mod mcp;
mod telegram;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
telepager — page a telegram user from an mcp client

usage: telepager [--config PATH]

  --config PATH   config file to use, instead of the default lookup
                  (./telepager.config.json, then telepager/config.json
                  in your user config dir)
  -h, --help      show this
  -V, --version   show the version
";

fn main() -> ExitCode {
    let config = match parse_args() {
        Ok(Action::Run(c)) => c,
        Ok(Action::Exit) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            eprint!("\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    // stdout is the mcp transport so logs have to go to stderr
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "info");
    }
    env_logger::init();

    match mcp::run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

enum Action {
    Run(Option<PathBuf>),
    Exit,
}

fn parse_args() -> Result<Action, String> {
    let mut config = None;
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
            "--config" => {
                let path = args.next().ok_or("--config needs a path")?;
                config = Some(PathBuf::from(path));
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }

    Ok(Action::Run(config))
}
