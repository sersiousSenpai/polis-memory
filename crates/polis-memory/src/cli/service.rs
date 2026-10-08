// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! `polis service install | uninstall | status`: keep `polis serve` running
//! as a per-user service — a launchd agent on macOS, a systemd user unit on
//! Linux. Without the daemon, captures are still recorded (the hook writes
//! the store directly), but nothing indexes them for semantic search, files
//! them, or takes the rotating backups.
//!
//! The service runs with the service manager's environment, not your shell's.
//! Install records the PATH it was run with, so the `claude` / `codex` CLI
//! the gardener files with is found as it is in your shell; a model key
//! exported in a shell profile is NOT carried, so no paid API is called
//! unless this home configures one.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::home::{current_exe, home_dir, Home};

/// The launchd label / systemd unit name.
pub const LABEL: &str = "com.polis-memory.serve";
pub const UNIT: &str = "polis-memory.service";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Launchd,
    Systemd,
}

impl Manager {
    pub fn current() -> Result<Manager, String> {
        if cfg!(target_os = "macos") {
            Ok(Manager::Launchd)
        } else if cfg!(target_os = "linux") {
            Ok(Manager::Systemd)
        } else {
            Err("`polis service` supports launchd (macOS) and systemd (Linux); run `polis serve` under your platform's service manager".into())
        }
    }

    /// Where the definition file lives for this user.
    pub fn definition_path(self, user_home: &Path) -> PathBuf {
        match self {
            Manager::Launchd => user_home.join("Library/LaunchAgents").join(format!("{LABEL}.plist")),
            Manager::Systemd => user_home.join(".config/systemd/user").join(UNIT),
        }
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The launchd agent: run at login, restart if it exits, log to the home.
pub fn launchd_plist(polis: &Path, home: &Home, path_env: &str) -> String {
    let log = home.root.join("logs").join("serve.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{polis}</string>
    <string>serve</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>POLIS_HOME</key><string>{home}</string>
    <key>PATH</key><string>{path_env}</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        polis = xml_escape(&polis.display().to_string()),
        home = xml_escape(&home.root.display().to_string()),
        log = xml_escape(&log.display().to_string()),
        path_env = xml_escape(path_env),
    )
}

/// The systemd user unit: started at login, restarted on failure.
pub fn systemd_unit(polis: &Path, home: &Home, path_env: &str) -> String {
    format!(
        "[Unit]\nDescription=Polis Memory daemon (polis serve)\n\n[Service]\nExecStart=\"{polis}\" serve\nEnvironment=\"POLIS_HOME={home}\"\nEnvironment=\"PATH={path_env}\"\nRestart=on-failure\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n",
        polis = polis.display(),
        home = home.root.display(),
        path_env = path_env.replace('"', ""),
    )
}

/// The PATH the service gets: this shell's, so the model CLIs resolve the
/// same way they do here.
pub fn current_path() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string())
}

/// The definition for this manager.
pub fn definition(manager: Manager, polis: &Path, home: &Home, path_env: &str) -> String {
    match manager {
        Manager::Launchd => launchd_plist(polis, home, path_env),
        Manager::Systemd => systemd_unit(polis, home, path_env),
    }
}

/// Write the definition (creating its directory and the log directory).
pub fn write_definition(manager: Manager, path: &Path, polis: &Path, home: &Home, path_env: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let _ = std::fs::create_dir_all(home.root.join("logs"));
    std::fs::write(path, definition(manager, polis, home, path_env)).map_err(|e| format!("write {}: {e}", path.display()))
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let shown = format!("{cmd:?}");
    let out = cmd.output().map_err(|e| format!("{shown}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{shown}: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

fn uid() -> Result<String, String> {
    let out = Command::new("id").arg("-u").output().map_err(|e| format!("id -u: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// What `install` / `uninstall` did.
#[derive(Debug, serde::Serialize)]
pub struct ServiceReport {
    pub manager: &'static str,
    pub definition: PathBuf,
    /// `install`: the service was loaded and started.
    pub started: bool,
    /// `uninstall`: a running service was stopped.
    pub stopped: bool,
}

fn manager_name(m: Manager) -> &'static str {
    match m {
        Manager::Launchd => "launchd",
        Manager::Systemd => "systemd",
    }
}

/// Write the definition and (unless `no_start`) load and start it.
pub fn install(home: &Home, polis: Option<PathBuf>, no_start: bool) -> Result<ServiceReport, String> {
    let manager = Manager::current()?;
    let user_home = home_dir().ok_or("HOME is not set")?;
    let path = manager.definition_path(&user_home);
    let polis = polis.unwrap_or_else(current_exe);
    if !no_start {
        // Replacing a loaded definition: stop the old one first (it may not
        // be loaded — that is fine).
        let _ = stop(manager, &path);
    }
    write_definition(manager, &path, &polis, home, &current_path())?;
    if !no_start {
        match manager {
            Manager::Launchd => run(Command::new("launchctl").arg("bootstrap").arg(format!("gui/{}", uid()?)).arg(&path))?,
            Manager::Systemd => {
                run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
                run(Command::new("systemctl").args(["--user", "enable", "--now", UNIT]))?;
            }
        }
    }
    Ok(ServiceReport { manager: manager_name(manager), definition: path, started: !no_start, stopped: false })
}

fn stop(manager: Manager, path: &Path) -> Result<(), String> {
    match manager {
        Manager::Launchd => run(Command::new("launchctl").arg("bootout").arg(format!("gui/{}", uid()?)).arg(path)),
        Manager::Systemd => run(Command::new("systemctl").args(["--user", "disable", "--now", UNIT])),
    }
}

/// Stop the service and remove its definition.
pub fn uninstall() -> Result<ServiceReport, String> {
    let manager = Manager::current()?;
    let user_home = home_dir().ok_or("HOME is not set")?;
    let path = manager.definition_path(&user_home);
    let stopped = path.exists() && stop(manager, &path).is_ok();
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("remove {}: {e}", path.display()))?;
    }
    if manager == Manager::Systemd {
        let _ = run(Command::new("systemctl").args(["--user", "daemon-reload"]));
    }
    Ok(ServiceReport { manager: manager_name(manager), definition: path, started: false, stopped })
}

/// Whether a definition is installed for this user.
pub fn installed() -> Option<PathBuf> {
    let manager = Manager::current().ok()?;
    let path = manager.definition_path(&home_dir()?);
    path.exists().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> Home {
        Home { root: PathBuf::from("/Users/a b/.polis") }
    }

    #[test]
    fn the_launchd_agent_runs_serve_for_this_home_and_escapes_paths() {
        let plist = launchd_plist(Path::new("/opt/Polis & Co/polis"), &home(), "/Users/a/.local/bin:/usr/bin");
        assert!(plist.contains("<string>/opt/Polis &amp; Co/polis</string>\n    <string>serve</string>"), "{plist}");
        assert!(plist.contains("<key>POLIS_HOME</key><string>/Users/a b/.polis</string>"));
        assert!(plist.contains("<key>KeepAlive</key><true/>"));
        assert!(plist.contains("<key>PATH</key><string>/Users/a/.local/bin:/usr/bin</string>"), "the model CLIs resolve as in the shell");
        assert!(plist.contains("/Users/a b/.polis/logs/serve.log"));
        assert!(plist.contains(&format!("<key>Label</key><string>{LABEL}</string>")));
    }

    #[test]
    fn the_systemd_unit_quotes_its_paths() {
        let unit = systemd_unit(Path::new("/home/a b/.local/bin/polis"), &home(), "/home/a/.local/bin:/usr/bin");
        assert!(unit.contains("ExecStart=\"/home/a b/.local/bin/polis\" serve\n"), "{unit}");
        assert!(unit.contains("Environment=\"POLIS_HOME=/Users/a b/.polis\"\n"));
        assert!(unit.contains("Environment=\"PATH=/home/a/.local/bin:/usr/bin\"\n"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn definitions_land_where_each_manager_reads_them() {
        let user = Path::new("/u");
        assert_eq!(Manager::Launchd.definition_path(user), Path::new("/u/Library/LaunchAgents/com.polis-memory.serve.plist"));
        assert_eq!(Manager::Systemd.definition_path(user), Path::new("/u/.config/systemd/user/polis-memory.service"));
        let dir = std::env::temp_dir().join(format!("polis-service-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let h = Home { root: dir.join("home") };
        let path = Manager::Systemd.definition_path(&dir);
        write_definition(Manager::Systemd, &path, Path::new("/bin/polis"), &h, "/usr/bin").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), systemd_unit(Path::new("/bin/polis"), &h, "/usr/bin"));
        assert!(h.root.join("logs").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
