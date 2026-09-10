// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Backups and restore (R13, Session E1): rotating `VACUUM INTO` snapshots,
//! each chain-verified after it is written, and a restore that re-verifies a
//! snapshot's chain before swapping it in.
//!
//! Host-neutral: the gardener runs [`backup_verify_prune`] on its cadence when
//! its config names a directory (the standalone daemon's), `polis backup` /
//! `polis restore` / `polis doctor` call the same functions, and a host with
//! its own cadence (Redline's keeper watch) can call [`backup_now`] directly.
//!
//! A snapshot is named `polis-<unix-ms>.db`; newest-first ordering is by that
//! number, never by mtime (a copied backup keeps its name, not its clock).

use std::path::{Path, PathBuf};

use polis_core::ledger::ChainVerdict;
use polis_store::PolisStore;

/// The cadence and retention the daemon's gardener runs: every 6 hours,
/// keep the newest 7 — Redline's `LEDGER_BACKUP_KEEP` and its 6 h watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupPolicy {
    pub every_ms: i64,
    pub keep: usize,
}

impl Default for BackupPolicy {
    fn default() -> Self {
        Self { every_ms: 6 * 60 * 60 * 1000, keep: 7 }
    }
}

/// The file name prefix every snapshot carries.
pub const SNAPSHOT_PREFIX: &str = "polis-";

/// What one backup did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupReport {
    pub path: PathBuf,
    /// The snapshot's chain, walked from the file just written — a backup
    /// that does not verify is reported, not trusted.
    pub verdict: ChainVerdict,
    /// Older snapshots removed to honour `keep`.
    pub pruned: usize,
}

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    /// The snapshot swapped in.
    pub from: PathBuf,
    /// Where the previous live file went (`<live>.bad`), when there was one.
    pub kept_as: Option<PathBuf>,
    /// The chain verdict of the snapshot, re-walked before the swap.
    pub verdict: ChainVerdict,
}

fn now_ms() -> i64 {
    polis_core::ledger::now_millis()
}

fn snapshot_stamp(path: &Path) -> Option<i64> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_prefix(SNAPSHOT_PREFIX)?.strip_suffix(".db")?;
    stem.parse().ok()
}

/// Every snapshot under `dir`, newest first (by the stamp in the name).
pub fn list_snapshots(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut stamped: Vec<(i64, PathBuf)> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter_map(|p| snapshot_stamp(&p).map(|s| (s, p)))
        .collect();
    stamped.sort_by_key(|(stamp, _)| std::cmp::Reverse(*stamp));
    stamped.into_iter().map(|(_, p)| p).collect()
}

/// Write one snapshot of `store` into `dir`. The store stays open and in
/// use; the copy is transactionally consistent (`VACUUM INTO`).
pub fn backup_now(store: &PolisStore, dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let mut stamp = now_ms();
    // Two backups in one millisecond (tests, a restart) must not collide.
    while dir.join(format!("{SNAPSHOT_PREFIX}{stamp}.db")).exists() {
        stamp += 1;
    }
    let dest = dir.join(format!("{SNAPSHOT_PREFIX}{stamp}.db"));
    store.snapshot_to(&dest).map_err(|e| format!("VACUUM INTO {}: {e}", dest.display()))?;
    Ok(dest)
}

/// Remove the oldest snapshots beyond `keep`. Returns how many went.
pub fn prune(dir: &Path, keep: usize) -> usize {
    let mut removed = 0;
    for old in list_snapshots(dir).into_iter().skip(keep) {
        if std::fs::remove_file(&old).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Open a snapshot read-only and walk its chain genesis → head. The open
/// itself checks the file is a Polis store at the current schema; the walk
/// is the trust criterion. (`PRAGMA quick_check` is NOT run here: SQLite's
/// FTS5 integrity pass needs a writable connection, and a snapshot is never
/// opened writable — `doctor` runs quick_check on the live file instead.)
pub fn verify_snapshot(path: &Path) -> Result<ChainVerdict, String> {
    let ro = PolisStore::open_read_only(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    ro.verify_ledger_chain().map_err(|e| format!("verify {}: {e}", path.display()))
}

/// The newest snapshot whose chain verifies (and whose file is sound).
pub fn newest_verifying(dir: &Path) -> Option<PathBuf> {
    list_snapshots(dir)
        .into_iter()
        .find(|p| verify_snapshot(p).map(|v| v.ok).unwrap_or(false))
}

/// The gardener's unit of work: snapshot, verify what was written, prune.
pub fn backup_verify_prune(store: &PolisStore, dir: &Path, keep: usize) -> Result<BackupReport, String> {
    let path = backup_now(store, dir)?;
    let verdict = verify_snapshot(&path)?;
    if !verdict.ok {
        tracing::warn!(path = %path.display(), first_bad_seq = ?verdict.first_bad_seq, "snapshot written but its chain does not verify");
    }
    let pruned = prune(dir, keep.max(1));
    Ok(BackupReport { path, verdict, pruned })
}

/// Swap a verifying snapshot in for the live file. `from` names one
/// explicitly; `None` takes the newest verifying snapshot under `dir`. The
/// snapshot's chain is re-walked first even when named (a file is trusted by
/// what it contains, not by where it sits), the live file is kept beside
/// itself as `<name>.bad` (its WAL / SHM sidecars removed, since they
/// belong to the old file), and the snapshot is COPIED in — the backup set
/// is left intact.
///
/// The caller must make sure nothing has the live file open (`polis serve`
/// stopped); the CLI checks the daemon before calling this.
pub fn restore(live: &Path, from: Option<&Path>, dir: &Path) -> Result<RestoreReport, String> {
    let from = match from {
        Some(p) => p.to_path_buf(),
        None => newest_verifying(dir).ok_or_else(|| format!("no verifying snapshot under {}", dir.display()))?,
    };
    let verdict = verify_snapshot(&from)?;
    if !verdict.ok {
        return Err(format!(
            "{} does not verify (first bad seq {:?}) — refusing to restore from it",
            from.display(),
            verdict.first_bad_seq
        ));
    }
    // The append-only hash tombstones sit outside the replaceable database.
    // A corrupt live file must not make restore lose its forgetting history.
    let markers = PathBuf::from(format!("{}.forgotten", live.display()));
    let mut forgotten = std::collections::BTreeSet::new();
    match std::fs::read_to_string(&markers) {
        Ok(text) => for hash in text.lines() {
            let valid = if let Some(target)=hash.strip_prefix("foreign_capture:") {
                target.rsplit_once(':').is_some_and(|(chain,seq)| chain.len()==64
                    && chain.bytes().all(|b|b.is_ascii_hexdigit()) && seq.parse::<i64>().is_ok_and(|n|n>0))
            } else {
                let digest=hash.strip_prefix("browse_event:").or_else(|| hash.strip_prefix("user_note:")).unwrap_or(hash);
                digest.len()==64 && digest.bytes().all(|b|b.is_ascii_hexdigit())
            };
            if !valid {
                return Err(format!("invalid forget registry {} — refusing resurrection risk", markers.display()));
            }
            forgotten.insert(hash.to_string());
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(format!("read forget registry: {e}")),
    }
    if live.exists() {
        if let Ok(current) = PolisStore::open_read_only(live) {
            let conn = current.conn();
            let mut stmt = conn.prepare("SELECT body_hash FROM forgotten_sources").map_err(|e| e.to_string())?;
            for hash in stmt.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())? {
                forgotten.insert(hash.map_err(|e| e.to_string())?);
            }
            let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='forgotten_captures')",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            if exists {
                let mut stmt=conn.prepare("SELECT target_kind || ':' || source_hash FROM forgotten_captures").map_err(|e|e.to_string())?;
                for marker in stmt.query_map([],|r|r.get::<_,String>(0)).map_err(|e|e.to_string())? {forgotten.insert(marker.map_err(|e|e.to_string())?);}
            }
            let mut stmt=conn.prepare("SELECT DISTINCT 'foreign_capture:' || target_chain || ':' || target_seq FROM foreign_redactions WHERE chain_id=target_chain
                UNION SELECT 'foreign_capture:' || replace(substr(key,length('polis.foreignForgotten.')+1),'.',':') FROM polis_meta WHERE key LIKE 'polis.foreignForgotten.%'").map_err(|e|e.to_string())?;
            for marker in stmt.query_map([],|r|r.get::<_,String>(0)).map_err(|e|e.to_string())? {forgotten.insert(marker.map_err(|e|e.to_string())?);}
        }
    }
    // Prepare and verify the sanitized replacement before moving the live
    // file, so copy/purge failures cannot leave the user without a database.
    let staged = live.with_extension(format!("restore-{}.db", std::process::id()));
    if staged.exists() { return Err(format!("restore staging file already exists: {}", staged.display())); }
    std::fs::copy(&from, &staged).map_err(|e| format!("stage snapshot: {e}"))?;
    {
        let replacement = PolisStore::open(&staged).map_err(|e| e.to_string())?;
        for hash in &forgotten {
            if let Some(target)=hash.strip_prefix("foreign_capture:") {
                let (chain,seq)=target.rsplit_once(':').ok_or("invalid foreign forget marker")?;
                replacement.tombstone_foreign_capture(chain,seq.parse().map_err(|_|"invalid foreign forget sequence")?).map_err(|e|e.to_string())?;
                continue;
            }
            if let Some(hash)=hash.strip_prefix("user_note:") {
                let ids = {
                    let conn=replacement.conn();
                    let mut stmt=conn.prepare("SELECT ne.note_id FROM note_events ne JOIN ledger_events e ON e.seq=ne.seq WHERE e.entry_hash=?1 AND e.payload_hash=ne.payload_hash").map_err(|e|e.to_string())?;
                    let rows=stmt.query_map([hash],|r|r.get::<_,i64>(0)).map_err(|e|e.to_string())?.collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?;
                    rows
                };
                for id in ids { replacement.forget_user_note(id,"restore").map_err(|e|e.to_string())?; }
                continue;
            }
            if let Some(hash)=hash.strip_prefix("browse_event:") {
                let ids={let conn=replacement.conn();let mut stmt=conn.prepare("SELECT id FROM browse_events WHERE context_hash=?1").map_err(|e|e.to_string())?;let rows=stmt.query_map([hash],|r|r.get::<_,i64>(0)).map_err(|e|e.to_string())?.collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?;rows};
                for id in ids {replacement.forget_browse_event(id,"restore").map_err(|e|e.to_string())?;}
                continue;
            }
            let ids = {
                let conn = replacement.conn();
                let mut stmt = conn.prepare("SELECT id FROM prompts WHERE body_hash = ?1").map_err(|e| e.to_string())?;
                let result = stmt.query_map([hash], |r| r.get::<_, i64>(0)).map_err(|e| e.to_string())?.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| e.to_string())?;
                result
            };
            for id in ids {
                replacement.compact_prompt_body(id, "[forgotten]", "forget", "deterministic", "restore").map_err(|e| e.to_string())?;
            }
        }
        if !replacement.verify_ledger_chain().map_err(|e| e.to_string())?.ok { return Err("sanitized restore failed chain verification".into()); }
        replacement.conn().execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").map_err(|e| e.to_string())?;
    }
    let kept_as = if live.exists() {
        let bad = live.with_extension("db.bad");
        let _ = std::fs::remove_file(&bad);
        std::fs::rename(live, &bad).map_err(|e| format!("set aside {}: {e}", live.display()))?;
        Some(bad)
    } else {
        None
    };
    for sidecar in ["-wal", "-shm", "-journal"] {
        let mut name = live.as_os_str().to_owned();
        name.push(sidecar);
        let _ = std::fs::remove_file(PathBuf::from(name));
    }
    std::fs::rename(&staged, live).map_err(|e| format!("install sanitized snapshot: {e}"))?;
    Ok(RestoreReport { from, kept_as, verdict })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PolisHandle;
    use polis_core::api::RememberRequest;
    use polis_core::host::NoHost;
    use polis_core::MemoryApi;
    use polis_llm::NoopSink;
    use std::sync::Arc;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("polis-backup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A store at `live` with `n` remembered prompts, through the same
    /// surface the CLI writes with.
    fn seeded(live: &Path, n: usize) -> Arc<PolisStore> {
        let store = Arc::new(PolisStore::open(live).unwrap());
        let handle = PolisHandle::new(store.clone(), None, Arc::new(NoHost), Arc::new(NoopSink));
        for i in 0..n {
            handle
                .remember(&RememberRequest {
                    text: format!("remember the postgres migration decision {i}"),
                    as_user: true,
                    ..Default::default()
                })
                .unwrap();
        }
        store
    }

    #[test]
    fn backup_verify_prune_keeps_the_newest_and_they_all_verify() {
        let dir = scratch("cadence");
        let store = seeded(&dir.join("polis.db"), 3);
        let backups = dir.join("backups");
        for _ in 0..4 {
            let r = backup_verify_prune(&store, &backups, 2).unwrap();
            assert!(r.verdict.ok);
            assert_eq!(r.verdict.checked, 3);
        }
        let left = list_snapshots(&backups);
        assert_eq!(left.len(), 2, "pruned to keep");
        assert!(snapshot_stamp(&left[0]) > snapshot_stamp(&left[1]), "newest first");
        assert_eq!(newest_verifying(&backups).as_deref(), Some(left[0].as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_swaps_in_the_newest_verifying_snapshot_and_keeps_the_bad_file() {
        let dir = scratch("restore");
        let live = dir.join("polis.db");
        let backups = dir.join("backups");
        {
            let store = seeded(&live, 5);
            backup_now(&store, &backups).unwrap();
        }
        // Corrupt the live file beyond quick_check.
        std::fs::write(&live, b"not a database at all").unwrap();
        for sidecar in ["polis.db-wal", "polis.db-shm"] {
            let _ = std::fs::remove_file(dir.join(sidecar));
        }
        let r = restore(&live, None, &backups).unwrap();
        assert!(r.verdict.ok);
        assert_eq!(r.verdict.checked, 5);
        assert_eq!(r.kept_as.as_deref(), Some(dir.join("polis.db.bad").as_path()));
        assert!(r.kept_as.as_ref().unwrap().exists());
        let back = PolisStore::open(&live).unwrap();
        assert!(back.verify_ledger_chain().unwrap().ok);
        assert_eq!(back.max_ledger_seq().unwrap(), 5);
        assert_eq!(list_snapshots(&backups).len(), 1, "the backup set is left intact");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tampered_snapshot_is_never_restored() {
        let dir = scratch("tamper");
        let live = dir.join("polis.db");
        let backups = dir.join("backups");
        let snap = {
            let store = seeded(&live, 2);
            backup_now(&store, &backups).unwrap()
        };
        // Rewrite one event's hash inside the snapshot.
        {
            let c = rusqlite::Connection::open(&snap).unwrap();
            c.execute("UPDATE ledger_events SET entry_hash = 'deadbeef' WHERE seq = 1", []).unwrap();
        }
        assert!(!verify_snapshot(&snap).unwrap().ok);
        assert_eq!(newest_verifying(&backups), None);
        let err = restore(&live, Some(&snap), &backups).unwrap_err();
        assert!(err.contains("does not verify"), "{err}");
        assert!(live.exists(), "the live file is untouched by a refused restore");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn managed_restore_preserves_forgetting_even_if_live_database_is_corrupt() {
        let dir = scratch("forget-restore");
        let live = dir.join("polis.db");
        let backups = dir.join("backups");
        let snapshot = {
            let store = seeded(&live, 1);
            let snapshot = backup_now(&store, &backups).unwrap();
            store.compact_prompt_body(1, "private migration", "cold", "deterministic", "test").unwrap();
            store.compact_prompt_body(1, "[forgotten]", "forget", "deterministic", "test").unwrap();
            snapshot
        };
        assert!(PathBuf::from(format!("{}.forgotten", live.display())).exists());
        std::fs::write(&live, b"corrupt database").unwrap();
        restore(&live, Some(&snapshot), &backups).unwrap();
        let store = PolisStore::open(&live).unwrap();
        let (body, gist): (String, Option<String>) = store.conn().query_row("SELECT body, gist FROM prompts WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert!(body.is_empty());
        assert_eq!(gist.as_deref(), Some("[forgotten]"));
        assert!(!store.restore_prompt_body(1).unwrap());
        assert_eq!(store.archive_stats().unwrap().0, 0);
        assert!(store.verify_ledger_chain().unwrap().ok);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }
}
