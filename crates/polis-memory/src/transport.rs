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
    /// E4: what other peers have reported holding of `chain_id`, as
    /// `(acker chain, acked seq)` — an org node relays these so an emitter
    /// learns of an ack from a subscriber whose chain it does not hold. A
    /// folder or a git remote carries acks inside segments and has nothing
    /// extra to say.
    async fn acks_for(&self, _chain_id: &str) -> Result<Vec<(String, i64)>, SyncError> {
        Ok(Vec::new())
    }
    /// E4: tell the transport what we hold after a fetch, signed — an org
    /// node records it so an emitter learns of the ack now, not on this
    /// peer's next published segment. A folder or a git remote has no one
    /// to tell.
    async fn report_acks(&self, _report: &polis_core::sync::AckReport) -> Result<(), SyncError> {
        Ok(())
    }
    /// E4: the org this transport publishes into (the node's principal id),
    /// stamped on published envelopes as `org_id`. None for a folder or a
    /// git remote.
    async fn org_id(&self) -> Option<String> {
        None
    }
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

// ---------------------------------------------------------------------------
// OrgNodeTransport (E4) — feature `orgnode`
// ---------------------------------------------------------------------------

/// An org node over HTTP: the same four calls against `/v1/sync/*`, with a
/// bearer token (an org node is never open). `ureq`, as the MCP remote
/// backend chose: the trait is async but the calls are short and blocking
/// is honest for a CLI.
#[cfg(feature = "orgnode")]
pub struct OrgNodeTransport {
    pub base: String,
    pub token: Option<String>,
    agent: ureq::Agent,
    node: std::sync::Mutex<Option<polis_core::sync::NodeCard>>,
}

#[cfg(feature = "orgnode")]
impl OrgNodeTransport {
    pub fn new(base: impl Into<String>, token: Option<String>) -> Self {
        let base = base.into();
        let base = base.trim_end_matches('/').to_string();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .http_status_as_error(false)
            .build()
            .into();
        OrgNodeTransport { base, token, agent, node: std::sync::Mutex::new(None) }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn get(&self, path: &str) -> Result<serde_json::Value, SyncError> {
        let mut req = self.agent.get(self.url(path));
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let mut resp = req.call().map_err(|e| SyncError::Io(format!("org node unreachable at {}: {e}", self.base)))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| SyncError::Io(e.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(SyncError::Io(format!("org node {path}: HTTP {status}: {text}")));
        }
        serde_json::from_str(&text).map_err(|e| SyncError::Parse(format!("{path}: {e}")))
    }

    fn card(&self) -> Result<polis_core::sync::NodeCard, SyncError> {
        if let Some(c) = self.node.lock().unwrap().clone() {
            return Ok(c);
        }
        let v = self.get("/v1/sync/chains")?;
        let card: polis_core::sync::NodeCard = serde_json::from_value(v["node"].clone()).map_err(|e| SyncError::Parse(format!("node card: {e}")))?;
        *self.node.lock().unwrap() = Some(card.clone());
        Ok(card)
    }
}

#[cfg(feature = "orgnode")]
#[async_trait]
impl SegmentTransport for OrgNodeTransport {
    fn key(&self) -> String {
        format!("org:{}", self.base)
    }
    async fn publish(&self, env: &Envelope) -> Result<SegmentRef, SyncError> {
        let mut req = self.agent.post(self.url("/v1/sync/segments"));
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let mut resp = req.send_json(env).map_err(|e| SyncError::Io(format!("org node unreachable at {}: {e}", self.base)))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| SyncError::Io(e.to_string()))?;
        if status == 409 {
            return Err(SyncError::Immutable(format!("the org node refused the segment as forked: {text}")));
        }
        if !(200..300).contains(&status) {
            return Err(SyncError::Io(format!("org node refused the segment: HTTP {status}: {text}")));
        }
        Ok(SegmentRef {
            chain_id: env.header.chain_id.clone(),
            from_seq: env.header.segment.from_seq,
            to_seq: env.header.segment.to_seq,
            locator: self.url(&format!("/v1/sync/segments/{}/{}/{}", env.header.chain_id, env.header.segment.from_seq, env.header.segment.to_seq)),
        })
    }
    async fn list(&self, chain_id: &str, after_seq: i64) -> Result<Vec<SegmentRef>, SyncError> {
        let v = self.get(&format!("/v1/sync/segments/{chain_id}?after={after_seq}"))?;
        let segs: Vec<polis_core::sync::SegmentSummary> = serde_json::from_value(v["segments"].clone()).map_err(|e| SyncError::Parse(e.to_string()))?;
        Ok(segs
            .into_iter()
            .map(|s| SegmentRef { locator: self.url(&format!("/v1/sync/segments/{}/{}/{}", s.chain_id, s.from_seq, s.to_seq)), chain_id: s.chain_id, from_seq: s.from_seq, to_seq: s.to_seq })
            .collect())
    }
    async fn fetch(&self, r: &SegmentRef) -> Result<Envelope, SyncError> {
        let path = r.locator.strip_prefix(&self.base).unwrap_or(&r.locator).to_string();
        let v = self.get(&path)?;
        serde_json::from_value(v).map_err(|e| SyncError::Parse(format!("{}: {e}", r.locator)))
    }
    async fn chains(&self, filter: &Subscription) -> Result<Vec<ChainCard>, SyncError> {
        let v = self.get("/v1/sync/chains")?;
        if let Ok(card) = serde_json::from_value::<polis_core::sync::NodeCard>(v["node"].clone()) {
            *self.node.lock().unwrap() = Some(card);
        }
        let chains: Vec<polis_core::sync::ChainSummary> = serde_json::from_value(v["chains"].clone()).map_err(|e| SyncError::Parse(e.to_string()))?;
        Ok(chains
            .into_iter()
            .map(|c| ChainCard { chain_id: c.chain_id, human_id: c.human_id, display_name: c.display_name, device_name: c.device_name, head_seq: c.head_seq })
            .filter(|c| filter.matches(c))
            .collect())
    }
    async fn acks_for(&self, chain_id: &str) -> Result<Vec<(String, i64)>, SyncError> {
        let v = self.get(&format!("/v1/sync/acks?chain={chain_id}"))?;
        let acks: Vec<polis_core::sync::AckSummary> = serde_json::from_value(v["acks"].clone()).map_err(|e| SyncError::Parse(e.to_string()))?;
        Ok(acks.into_iter().map(|a| (a.acker_chain, a.acked_seq)).collect())
    }
    async fn report_acks(&self, report: &polis_core::sync::AckReport) -> Result<(), SyncError> {
        let mut req = self.agent.post(self.url("/v1/sync/acks"));
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let mut resp = req.send_json(report).map_err(|e| SyncError::Io(format!("org node unreachable at {}: {e}", self.base)))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| SyncError::Io(e.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(SyncError::Io(format!("org node refused the ack report: HTTP {status}: {text}")));
        }
        Ok(())
    }
    async fn org_id(&self) -> Option<String> {
        self.card().ok().map(|c| c.principal_id)
    }
}
