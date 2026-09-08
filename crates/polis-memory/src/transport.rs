// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! `SegmentTransport` (Session E3, plan §4.6): where signed segments travel.
//! One trait, the envelope and the verifier do the hard work; a transport
//! is a pipe. Two ship here — a folder (iCloud/Dropbox/NFS for a solo user
//! with two machines) and a git remote (an OSS team's repo) — and the org
//! node (E4) implements the same four calls over HTTPS.
//!
//! Layout, both transports: `<root>/<chain_id>/<from>-<to>.polis.json`,
//! append-only — a segment file is never rewritten (a peer that sees a
//! changed file would see a fork, which is the point).

use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::envelope::Envelope;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentRef {
    pub chain_id: String,
    pub from_seq: i64,
    pub to_seq: i64,
    /// Transport-specific locator (a path, a URL).
    pub locator: String,
}

/// What a transport knows about a chain before any segment is fetched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainCard {
    pub chain_id: String,
    pub human_id: String,
    pub display_name: Option<String>,
    pub device_name: Option<String>,
    /// The newest `to_seq` published.
    pub head_seq: i64,
}

/// A selective subscription: empty lists match everything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    /// Human ids, fingerprint prefixes or display names.
    pub principals: Vec<String>,
    pub projects: Vec<String>,
    pub classes: Vec<String>,
}

impl Subscription {
    pub fn matches(&self, card: &ChainCard) -> bool {
        if self.principals.is_empty() {
            return true;
        }
        self.principals.iter().any(|p| {
            let p_l = p.trim().to_ascii_lowercase();
            !p_l.is_empty()
                && (card.human_id == p_l
                    || card.human_id.starts_with(&p_l)
                    || polis_core::identity::fingerprint(&card.human_id) == p_l
                    || card.display_name.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(p.trim())))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum SyncError {
    Io(String),
    Parse(String),
    /// Refused to overwrite a segment file with different bytes.
    Immutable(String),
    Git(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Io(s) => write!(f, "io: {s}"),
            SyncError::Parse(s) => write!(f, "parse: {s}"),
            SyncError::Immutable(s) => write!(f, "immutable: {s}"),
            SyncError::Git(s) => write!(f, "git: {s}"),
        }
    }
}

#[async_trait]
pub trait SegmentTransport: Send + Sync {
    /// A short stable key for this transport (the high-water mark is kept
    /// per key).
    fn key(&self) -> String;
    async fn publish(&self, env: &Envelope) -> Result<SegmentRef, SyncError>;
    /// Segments of `chain_id` that extend past `after_seq`, oldest first.
    async fn list(&self, chain_id: &str, after_seq: i64) -> Result<Vec<SegmentRef>, SyncError>;
    async fn fetch(&self, r: &SegmentRef) -> Result<Envelope, SyncError>;
    async fn chains(&self, filter: &Subscription) -> Result<Vec<ChainCard>, SyncError>;
}

// ---------------------------------------------------------------------------
// FolderTransport
// ---------------------------------------------------------------------------

/// A directory: `<dir>/<chain_id>/<from>-<to>.polis.json`.
pub struct FolderTransport {
    pub dir: PathBuf,
}

impl FolderTransport {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        FolderTransport { dir: dir.into() }
    }

    fn file_name(from: i64, to: i64) -> String {
        format!("{from:012}-{to:012}.polis.json")
    }

    fn parse_name(name: &str) -> Option<(i64, i64)> {
        let stem = name.strip_suffix(".polis.json")?;
        let (a, b) = stem.split_once('-')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    }

    /// The folder's segments for a chain, oldest first.
    pub fn segments_in(dir: &Path, chain_id: &str) -> Result<Vec<SegmentRef>, SyncError> {
        let chain_dir = dir.join(chain_id);
        if !chain_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&chain_dir).map_err(|e| SyncError::Io(format!("{}: {e}", chain_dir.display())))? {
            let entry = entry.map_err(|e| SyncError::Io(e.to_string()))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some((from, to)) = Self::parse_name(&name) {
                out.push(SegmentRef { chain_id: chain_id.to_string(), from_seq: from, to_seq: to, locator: entry.path().to_string_lossy().to_string() });
            }
        }
        out.sort_by_key(|r| (r.from_seq, r.to_seq));
        Ok(out)
    }

    pub fn publish_to(dir: &Path, env: &Envelope) -> Result<SegmentRef, SyncError> {
        let chain_dir = dir.join(&env.header.chain_id);
        std::fs::create_dir_all(&chain_dir).map_err(|e| SyncError::Io(format!("{}: {e}", chain_dir.display())))?;
        let path = chain_dir.join(Self::file_name(env.header.segment.from_seq, env.header.segment.to_seq));
        let text = serde_json::to_string(env).map_err(|e| SyncError::Parse(e.to_string()))?;
        if path.exists() {
            let existing = std::fs::read_to_string(&path).map_err(|e| SyncError::Io(e.to_string()))?;
            if existing != text {
                return Err(SyncError::Immutable(format!("{} exists with different bytes — a segment is never rewritten", path.display())));
            }
        } else {
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, &text).map_err(|e| SyncError::Io(format!("{}: {e}", tmp.display())))?;
            std::fs::rename(&tmp, &path).map_err(|e| SyncError::Io(format!("{}: {e}", path.display())))?;
        }
        Ok(SegmentRef { chain_id: env.header.chain_id.clone(), from_seq: env.header.segment.from_seq, to_seq: env.header.segment.to_seq, locator: path.to_string_lossy().to_string() })
    }

    pub fn read(r: &SegmentRef) -> Result<Envelope, SyncError> {
        let text = std::fs::read_to_string(&r.locator).map_err(|e| SyncError::Io(format!("{}: {e}", r.locator)))?;
        serde_json::from_str(&text).map_err(|e| SyncError::Parse(format!("{}: {e}", r.locator)))
    }

    pub fn chains_in(dir: &Path, filter: &Subscription) -> Result<Vec<ChainCard>, SyncError> {
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).map_err(|e| SyncError::Io(format!("{}: {e}", dir.display())))? {
            let entry = entry.map_err(|e| SyncError::Io(e.to_string()))?;
            if !entry.path().is_dir() {
                continue;
            }
            let chain_id = entry.file_name().to_string_lossy().to_string();
            let segs = Self::segments_in(dir, &chain_id)?;
            let Some(newest) = segs.last() else { continue };
            // The card comes from the newest segment's header — one read.
            let env = Self::read(newest)?;
            let card = ChainCard {
                chain_id: chain_id.clone(),
                human_id: env.header.principal.human.principal_id.clone(),
                display_name: env.header.principal.human.display_name.clone(),
                device_name: env.header.principal.device.display_name.clone(),
                head_seq: newest.to_seq,
            };
            if filter.matches(&card) {
                out.push(card);
            }
        }
        out.sort_by(|a, b| a.chain_id.cmp(&b.chain_id));
        Ok(out)
    }
}

#[async_trait]
impl SegmentTransport for FolderTransport {
    fn key(&self) -> String {
        format!("folder:{}", self.dir.display())
    }
    async fn publish(&self, env: &Envelope) -> Result<SegmentRef, SyncError> {
        Self::publish_to(&self.dir, env)
    }
    async fn list(&self, chain_id: &str, after_seq: i64) -> Result<Vec<SegmentRef>, SyncError> {
        Ok(Self::segments_in(&self.dir, chain_id)?.into_iter().filter(|r| r.to_seq > after_seq).collect())
    }
    async fn fetch(&self, r: &SegmentRef) -> Result<Envelope, SyncError> {
        Self::read(r)
    }
    async fn chains(&self, filter: &Subscription) -> Result<Vec<ChainCard>, SyncError> {
        Self::chains_in(&self.dir, filter)
    }
}

// ---------------------------------------------------------------------------
// GitTransport
// ---------------------------------------------------------------------------

/// A git remote as the folder: a working clone under `work_dir`, pulled
/// before every read, committed and pushed after every publish. Shells out
/// to the `git` binary — no crate: the layout is plain files, every dev
/// machine has git, and a pure-Rust git stack would be the heaviest
/// dependency in the graph for four commands.
pub struct GitTransport {
    pub remote: String,
    pub work_dir: PathBuf,
    pub branch: String,
}

impl GitTransport {
    pub fn new(remote: impl Into<String>, work_dir: impl Into<PathBuf>) -> Self {
        GitTransport { remote: remote.into(), work_dir: work_dir.into(), branch: "main".into() }
    }

    fn git(&self, args: &[&str]) -> Result<String, SyncError> {
        let out = Command::new("git")
            .args(["-c", "user.name=polis", "-c", "user.email=polis@localhost", "-c", "commit.gpgsign=false"])
            .arg("-C")
            .arg(&self.work_dir)
            .args(args)
            .output()
            .map_err(|e| SyncError::Git(format!("spawn git: {e}")))?;
        if !out.status.success() {
            return Err(SyncError::Git(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim())));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Clone if the working directory is not a clone yet.
    pub fn ensure_clone(&self) -> Result<(), SyncError> {
        if self.work_dir.join(".git").exists() {
            return Ok(());
        }
        if let Some(parent) = self.work_dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SyncError::Io(e.to_string()))?;
        }
        let out = Command::new("git")
            .args(["clone", "--quiet", &self.remote])
            .arg(&self.work_dir)
            .output()
            .map_err(|e| SyncError::Git(format!("spawn git: {e}")))?;
        if !out.status.success() {
            return Err(SyncError::Git(format!("git clone {}: {}", self.remote, String::from_utf8_lossy(&out.stderr).trim())));
        }
        // An empty remote clones with no branch; give the work tree ours.
        let _ = self.git(&["checkout", "-q", "-B", &self.branch]);
        Ok(())
    }

    /// Bring the clone up to date. An empty remote (no branch yet) is not
    /// an error — there is nothing to pull.
    pub fn pull(&self) -> Result<(), SyncError> {
        self.ensure_clone()?;
        let remote_has_branch = self.git(&["ls-remote", "--heads", "origin", &self.branch]).map(|s| !s.trim().is_empty()).unwrap_or(false);
        if remote_has_branch {
            self.git(&["fetch", "-q", "origin", &self.branch])?;
            let _ = self.git(&["checkout", "-q", "-B", &self.branch]);
            self.git(&["reset", "-q", "--hard", &format!("origin/{}", self.branch)])?;
        }
        Ok(())
    }

    fn push(&self, message: &str) -> Result<(), SyncError> {
        self.git(&["add", "-A"])?;
        // Nothing staged → nothing to commit → nothing to push.
        if self.git(&["diff", "--cached", "--quiet"]).is_ok() {
            return Ok(());
        }
        self.git(&["commit", "-q", "-m", message])?;
        self.git(&["push", "-q", "origin", &format!("HEAD:{}", self.branch)])?;
        Ok(())
    }
}

#[async_trait]
impl SegmentTransport for GitTransport {
    fn key(&self) -> String {
        format!("git:{}", self.remote)
    }
    async fn publish(&self, env: &Envelope) -> Result<SegmentRef, SyncError> {
        self.pull()?;
        let r = FolderTransport::publish_to(&self.work_dir, env)?;
        self.push(&format!("polis: {} {}-{}", polis_core::identity::fingerprint(&env.header.chain_id), env.header.segment.from_seq, env.header.segment.to_seq))?;
        Ok(r)
    }
    async fn list(&self, chain_id: &str, after_seq: i64) -> Result<Vec<SegmentRef>, SyncError> {
        self.pull()?;
        Ok(FolderTransport::segments_in(&self.work_dir, chain_id)?.into_iter().filter(|r| r.to_seq > after_seq).collect())
    }
    async fn fetch(&self, r: &SegmentRef) -> Result<Envelope, SyncError> {
        FolderTransport::read(r)
    }
    async fn chains(&self, filter: &Subscription) -> Result<Vec<ChainCard>, SyncError> {
        self.pull()?;
        FolderTransport::chains_in(&self.work_dir, filter)
    }
}
