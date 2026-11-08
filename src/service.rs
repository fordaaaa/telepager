use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The launchd label / filename stem used by the service.
pub const LABEL: &str = "com.telepager.daemon";

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn plist_string(s: &str) -> String {
    format!("<string>{}</string>", xml_escape(s))
}

fn log_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join("Library/Logs/telepager.log"))
        .unwrap_or_else(|| std::env::temp_dir().join("telepager.log"))
}

/// A macOS launch agent that runs `telepager start --no-open` at login.
///
/// Pure so tests can check the unit without touching a real LaunchAgents dir.
pub fn launch_agent_plist(exe: &Path, config: Option<&Path>) -> String {
    let mut argv = vec![
        exe.to_string_lossy().into_owned(),
        "start".to_string(),
        "--no-open".to_string(),
    ];
    if let Some(config) = config {
        argv.push("--config".to_string());
        argv.push(config.to_string_lossy().into_owned());
    }
    let arguments: String = argv
        .iter()
        .map(|a| format!("        {}\n", plist_string(a)))
        .collect();
    let log = plist_string(&log_path().to_string_lossy());

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    {label}
    <key>ProgramArguments</key>
    <array>
{arguments}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>StandardOutPath</key>
    {log}
    <key>StandardErrorPath</key>
    {log}
</dict>
</plist>
"#,
        label = plist_string(LABEL),
    )
}

/// Quote an argument for a systemd `ExecStart=` line.
fn systemd_quote(text: &str) -> String {
    if text.contains([' ', '"', '\\']) {
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        text.to_string()
    }
}

/// A systemd user unit that runs `telepager start --no-open` at login.
///
/// Pure for the same reason as [`launch_agent_plist`].
pub fn systemd_unit(exe: &Path, config: Option<&Path>) -> String {
    let mut exec = format!(
        "{} start --no-open",
        systemd_quote(&exe.to_string_lossy())
    );
    if let Some(config) = config {
        exec.push_str(" --config ");
        exec.push_str(&systemd_quote(&config.to_string_lossy()));
    }

    format!(
        r#"[Unit]
Description=telepager daemon
After=network-online.target

[Service]
Type=simple
ExecStart={exec}
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"#
    )
}

#[cfg(target_os = "macos")]
fn plist_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn unit_path() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    config.join("systemd").join("user").join("telepager.service")
}

#[cfg(target_os = "macos")]
fn uid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

/// `telepager service install`: register a login/boot service that runs
/// `telepager start --no-open`. Idempotent — an existing unit is replaced.
pub fn install(config: Option<PathBuf>) -> Result<()> {
    let exe = std::env::current_exe().context("finding our own binary")?;

    #[cfg(target_os = "macos")]
    {
        let path = plist_path().context("finding your LaunchAgents directory")?;
        write_unit(&path, &launch_agent_plist(&exe, config.as_deref()))?;

        // unload any previous copy so the new one is what runs
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &format!("gui/{}/{LABEL}", uid())])
            .status();

        let loaded = std::process::Command::new("launchctl")
            .args(["load", "-w"])
            .arg(&path)
            .status()
            .context("running launchctl load")?
            .success();
        if !loaded {
            anyhow::bail!(
                "launchctl couldn't load {}. try `launchctl load -w {}` yourself.",
                path.display(),
                path.display()
            );
        }
        println!("installed {}.", path.display());
        println!("telepager will start in the background at login.");
        return Ok(());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let path = unit_path();
        write_unit(&path, &systemd_unit(&exe, config.as_deref()))?;

        run_status("systemctl", &["--user", "daemon-reload"])?;
        run_status("systemctl", &["--user", "enable", "--now", "telepager.service"])?;
        println!("installed {}.", path.display());
        println!("telepager will start in the background at login.");
        return Ok(());
    }

    #[cfg(windows)]
    {
        let _ = (exe, config);
        anyhow::bail!(
            "`telepager service` isn't supported on Windows yet. \
             Add `telepager start --no-open` as a logon task in Task Scheduler."
        );
    }
}

/// `telepager service uninstall`: remove the service and disable it.
pub fn uninstall() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let Some(path) = plist_path() else {
            println!("no service installed.");
            return Ok(());
        };
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &format!("gui/{}/{LABEL}", uid())])
            .status();
        let _ = std::process::Command::new("launchctl").args(["unload"]).arg(&path).status();
        remove_unit(&path)?;
        println!("uninstalled. telepager won't start at login anymore.");
        return Ok(());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let path = unit_path();
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "disable", "--now", "telepager.service"])
            .status();
        remove_unit(&path)?;
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        println!("uninstalled. telepager won't start at login anymore.");
        return Ok(());
    }

    #[cfg(windows)]
    {
        anyhow::bail!("`telepager service` isn't supported on Windows yet.");
    }
}

#[cfg(unix)]
fn write_unit(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

#[cfg(unix)]
fn remove_unit(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("no service installed.");
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn run_status(program: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("running {program} {}", args.join(" ")))?;
    if !status.success() {
        anyhow::bail!("{program} {} failed", args.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn exe() -> PathBuf {
        PathBuf::from("/opt/tele pager/bin/telepager")
    }

    #[test]
    fn the_plist_starts_the_daemon_at_login() {
        let plist = launch_agent_plist(&exe(), None);
        assert!(plist.contains(LABEL), "{plist}");
        assert!(plist.contains("/opt/tele pager/bin/telepager"), "{plist}");
        assert!(plist.contains("<string>start</string>"), "{plist}");
        assert!(plist.contains("<string>--no-open</string>"), "{plist}");
        assert!(plist.contains("<key>RunAtLoad</key>"), "{plist}");
        assert!(!plist.contains("--config"), "{plist}");
    }

    #[test]
    fn the_plist_carries_a_config_path() {
        let plist = launch_agent_plist(&exe(), Some(Path::new("/tmp/cfg.json")));
        assert!(plist.contains("<string>--config</string>"), "{plist}");
        assert!(plist.contains("/tmp/cfg.json"), "{plist}");
        // paths with & must not break the xml
        let odd = launch_agent_plist(Path::new("/tmp/a&b"), None);
        assert!(odd.contains("/tmp/a&amp;b"), "{odd}");
    }

    #[test]
    fn the_systemd_unit_starts_the_daemon_at_login() {
        let unit = systemd_unit(&exe(), None);
        assert!(unit.contains("ExecStart="), "{unit}");
        assert!(unit.contains("/opt/tele pager/bin/telepager"), "{unit}");
        assert!(unit.contains(" start --no-open"), "{unit}");
        assert!(unit.contains("Restart=on-failure"), "{unit}");
        assert!(unit.contains("WantedBy=default.target"), "{unit}");
        assert!(!unit.contains("--config"), "{unit}");
    }

    #[test]
    fn the_systemd_unit_carries_a_config_path() {
        let unit = systemd_unit(&exe(), Some(Path::new("/tmp/cfg.json")));
        assert!(unit.contains("--config"), "{unit}");
        assert!(unit.contains("/tmp/cfg.json"), "{unit}");
    }
}
