// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! `$POLIS_HOME` (default `~/.polis`), §4.7: `config.toml`, `token` (persistent,
//! 0600), `polis.db`, `serve.json`, `gardener.lock`, `backups/`. Env:
//! `POLIS_HOME`, `POLIS_DB`, `POLIS_TOKEN`, `POLIS_REMOTE`, `POLIS_AGENT`,
//! `POLIS_NO_NETWORK`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The standalone daemon's default address.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7677";

#[derive(Debug, Clone)]
pub struct Home {
    pub root: PathBuf,
}

/// What a running `polis serve` leaves behind for `polis mcp` and `polis
/// capture` to find it by.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServeInfo {
    pub pid: u32,
    pub addr: String,
    pub token_path: String,
    pub started_at: i64,
}

impl ServeInfo {
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn home_dir() -> Option<PathBuf> {
    env_nonempty("HOME").or_else(|| env_nonempty("USERPROFILE")).map(PathBuf::from)
}

impl Home {
    /// `POLIS_HOME`, else `~/.polis`.
    pub fn resolve() -> Result<Home, String> {
        if let Some(h) = env_nonempty("POLIS_HOME") {
            return Ok(Home { root: PathBuf::from(h) });
        }
        home_dir()
            .map(|h| Home { root: h.join(".polis") })
            .ok_or_else(|| "neither POLIS_HOME nor HOME/USERPROFILE is set".to_string())
    }

    /// `POLIS_DB`, else config.toml `db` (an adopted Redline store), else
    /// `polis.db` in the home.
    pub fn db_path(&self) -> PathBuf {
        env_nonempty("POLIS_DB")
            .or_else(|| self.config_get("db"))
            .map(PathBuf::from)
            .unwrap_or_else(|| self.root.join("polis.db"))
    }

    /// Where `identity.key` lives: config.toml `identity_dir` (an adopted
    /// Redline install shares its key), else the home.
    pub fn identity_dir(&self) -> PathBuf {
        self.config_get("identity_dir").map(PathBuf::from).unwrap_or_else(|| self.root.clone())
    }

    /// The device name: config.toml `device`, else the hostname.
    pub fn device_name(&self) -> String {
        self.config_get("device").unwrap_or_else(crate::identity::default_device_name)
    }

    pub fn config_get(&self, key: &str) -> Option<String> {
        std::fs::read_to_string(self.config_path()).ok().and_then(|t| config_value(&t, key))
    }

    /// Set one `key = "value"` line in config.toml (replacing an existing
    /// line for the key, else appending). Creates the file if needed.
    pub fn config_set(&self, key: &str, value: &str) -> Result<(), String> {
        let path = self.config_path();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut out = String::new();
        let mut replaced = false;
        for line in text.lines() {
            let is_key = line.trim().split_once('=').map(|(k, _)| k.trim() == key).unwrap_or(false) && !line.trim().starts_with('#');
            if is_key {
                if !replaced {
                    out.push_str(&format!("{key} = \"{value}\"\n"));
                    replaced = true;
                }
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        if !replaced {
            out.push_str(&format!("{key} = \"{value}\"\n"));
        }
        std::fs::write(&path, out).map_err(|e| format!("write {}: {e}", path.display()))
    }
    pub fn token_path(&self) -> PathBuf {
        self.root.join("token")
    }
    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }
    pub fn serve_path(&self) -> PathBuf {
        self.root.join("serve.json")
    }
    pub fn lock_path(&self) -> PathBuf {
        self.root.join("gardener.lock")
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }

    pub fn exists(&self) -> bool {
        self.root.is_dir()
    }

    /// Create the directory (owner-only on unix).
    pub fn ensure(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.root).map_err(|e| format!("create {}: {e}", self.root.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700));
        }
        Ok(())
    }

    /// `POLIS_TOKEN`, else the persistent token file.
    pub fn read_token(&self) -> Option<String> {
        env_nonempty("POLIS_TOKEN").or_else(|| std::fs::read_to_string(self.token_path()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
    }

    /// The token, generated on first use (64 hex chars, written 0600 before
    /// the bytes land).
    pub fn ensure_token(&self) -> Result<String, String> {
        if let Some(t) = self.read_token() {
            return Ok(t);
        }
        let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        write_private(&self.token_path(), token.as_bytes())?;
        Ok(token)
    }

    pub fn read_serve(&self) -> Option<ServeInfo> {
        let text = std::fs::read_to_string(self.serve_path()).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn write_serve(&self, info: &ServeInfo) -> Result<(), String> {
        let text = serde_json::to_string_pretty(info).map_err(|e| e.to_string())?;
        write_private(&self.serve_path(), text.as_bytes())
    }

    pub fn remove_serve(&self) {
        let _ = std::fs::remove_file(self.serve_path());
    }

    /// The configured listen address (`listen = "…"` in config.toml, else the
    /// default). The file is a handful of `key = "value"` lines; read as such.
    pub fn listen(&self) -> String {
        std::fs::read_to_string(self.config_path())
            .ok()
            .and_then(|text| config_value(&text, "listen"))
            .unwrap_or_else(|| DEFAULT_LISTEN.to_string())
    }

    /// Write the default `config.toml` if there is none.
    pub fn ensure_config(&self) -> Result<bool, String> {
        let path = self.config_path();
        if path.exists() {
            return Ok(false);
        }
        let text = format!(
            "# Polis Memory — {}\n#\n# listen: where `polis serve` binds. Loopback by default; a non-loopback\n# address needs `polis serve --token-file` (or the writes are open to the\n# network).\nlisten = \"{}\"\n",
            self.root.display(),
            DEFAULT_LISTEN
        );
        std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
        Ok(true)
    }
}

/// `key = "value"` from a minimal TOML-shaped file (comments and blank lines
/// ignored). Enough for `config.toml` until it grows a real parser.
pub fn config_value(text: &str, key: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == key).then(|| v.trim().trim_matches('"').to_string())
        })
        .filter(|v| !v.is_empty())
}

/// Write a file the owner alone can read (0600 on unix; the parent's ACL on
/// Windows until the owner-only ACL lands with identity in E2).
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    let mut f = opts.open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    f.write_all(bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// The absolute path of this binary — what the hook and the client configs
/// are written with.
pub fn current_exe() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.canonicalize().ok()).unwrap_or_else(|| PathBuf::from("polis"))
}

/// Is `name` on PATH?
pub fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) { vec![String::new(), ".exe".into(), ".cmd".into(), ".bat".into()] } else { vec![String::new()] };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_values_read_and_the_default_listen_is_loopback() {
        let text = "# comment\nlisten = \"127.0.0.1:9000\"\nother=\"x\"\n";
        assert_eq!(config_value(text, "listen").as_deref(), Some("127.0.0.1:9000"));
        assert_eq!(config_value(text, "other").as_deref(), Some("x"));
        assert_eq!(config_value(text, "missing"), None);
        assert!(DEFAULT_LISTEN.starts_with("127.0.0.1:"));
    }

    #[test]
    fn the_token_is_generated_once_and_private() {
        let root = std::env::temp_dir().join(format!("polis-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = Home { root: root.clone() };
        home.ensure().unwrap();
        let t1 = home.ensure_token().unwrap();
        let t2 = home.ensure_token().unwrap();
        assert_eq!(t1, t2);
        assert_eq!(t1.len(), 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(home.token_path()).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(home.ensure_config().unwrap());
        assert!(!home.ensure_config().unwrap());
        assert_eq!(home.listen(), DEFAULT_LISTEN);
        let info = ServeInfo { pid: 1, addr: "127.0.0.1:7677".into(), token_path: "t".into(), started_at: 5 };
        home.write_serve(&info).unwrap();
        assert_eq!(home.read_serve(), Some(info));
        home.remove_serve();
        assert_eq!(home.read_serve(), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
