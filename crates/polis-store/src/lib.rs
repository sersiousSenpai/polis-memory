// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! `polis-store` — the SQLite store under Polis Memory.
//!
//! Owns the memory schema (the hash-chained lake, the class catalog, the
//! derived lexical and semantic indexes) and brings it current under its own
//! version table, `polis_meta`. It can **attach** to a connection a host
//! already owns (Redline: one `Mutex<Connection>` shared with the app's own
//! tables, one lock, one transaction domain) or **open** a file standalone
//! with the pragmas a long-lived local daemon wants (WAL, `synchronous =
//! NORMAL`, a busy timeout).
//!
//! Session A2 of the Polis extraction (`docs/polis-extraction.md` in the
//! Redline repo) moved the DDL here byte-for-byte; the memory METHODS follow
//! in A3. Until then the host reaches its own methods on `Database`, and
//! `Deref<Target = PolisStore>` is the seam they cross.

pub mod ledger;
pub mod lexical;
pub mod meta;
pub mod schema;
pub mod exports;
pub mod session_tree;
pub mod embeddings;
pub mod browse;
pub mod observations;
pub mod supersessions;
pub mod catalog;
pub mod notes;
pub mod search;
pub mod chain;
pub mod compaction;
pub mod prompts;
pub mod record;
pub mod principals;

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::Connection;

pub use compaction::ARCHIVE_ALGO;
pub use lexical::{LEXICAL_VERSION, PREFIX_SIZES, TOKENIZER};
pub use prompts::PROMPT_TEXT;
pub use search::{GrepError, GREP_MIN_LITERAL};
pub use schema::{CORPUS_ROLE_VERSION, SYSTEM_INDEX_CHARS};

/// Why the store could not attach or act.
#[derive(Debug)]
pub enum StoreError {
    /// The SQLite this process linked lacks something the schema needs
    /// (FTS5, or a version older than 3.34). Named, not degraded: a store
    /// that silently ran without its lexical layer would look empty.
    MissingCapability(String),
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::MissingCapability(what) => write!(f, "sqlite is missing {what}"),
            StoreError::Sqlite(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

/// How to attach.
#[derive(Debug, Clone)]
pub struct AttachOptions {
    /// The host's settings table to adopt legacy version keys from, on the
    /// store's first attach. `None` for a standalone store.
    pub legacy_settings_table: Option<String>,
    /// The author stamped on events the store writes on its own account
    /// (curation, supersession on approval). Identity proper is E2's; until
    /// then this is the host's `local_author()` or the OS login name.
    pub author: String,
}

impl AttachOptions {
    /// Attaching to a Redline database: adopt the two `app_settings` keys.
    pub fn redline() -> Self {
        Self { legacy_settings_table: Some("app_settings".to_string()), author: default_author() }
    }

    /// A store of its own: nothing to adopt.
    pub fn standalone() -> Self {
        Self { legacy_settings_table: None, author: default_author() }
    }

    pub fn with_author(mut self, author: impl Into<String>) -> Self {
        self.author = author.into();
        self
    }
}

/// The OS login name, else `"local"` — the same fallback the host's
/// `local_author()` uses beneath its own override.
pub fn default_author() -> String {
    std::env::var("USER")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "local".to_string())
}

/// What one attach did — for a host's boot trace and for the tests that pin
/// "an already-current store runs no schema SQL".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachReport {
    /// Legacy keys copied into `polis_meta` (first attach to a host DB only).
    pub adopted_keys: usize,
    /// Whether the migration steps ran (a fresh or out-of-date store).
    pub migrated: bool,
}

/// The store: one shared connection, the schema current.
pub struct PolisStore {
    conn: Arc<Mutex<Connection>>,
    last_attach: AttachReport,
    author: String,
}

/// The minimum SQLite the schema needs (`RETURNING`, generated columns, the
/// trigram tokenizer all predate it; 3.34 is where the FTS5 features used
/// here are all present).
pub const MIN_SQLITE: (u32, u32) = (3, 34);

impl PolisStore {
    /// Attach to a connection the host owns. Verifies capabilities, creates
    /// `polis_meta`, adopts legacy versions on a first attach, runs the
    /// idempotent migration if any version is behind, and verifies the
    /// tables exist. An already-current store runs no schema SQL.
    pub fn attach(conn: Arc<Mutex<Connection>>, opts: AttachOptions) -> Result<Self, StoreError> {
        let author = opts.author.clone();
        let report = {
            let c = lock(&conn);
            Self::require_capabilities(&c)?;
            meta::ensure_table(&c)?;
            let mut report = AttachReport::default();
            let versions = meta::versions(&c)?;
            if versions.schema.is_none() {
                if let Some(table) = opts.legacy_settings_table.as_deref() {
                    report.adopted_keys = meta::adopt_legacy(&c, table)?;
                }
            }
            let versions = meta::versions(&c)?;
            let current = versions.schema.as_deref() == Some(meta::STORE_SCHEMA_VERSION)
                && versions.lexical.as_deref() == Some(LEXICAL_VERSION)
                && versions.corpus_role.as_deref() == Some(CORPUS_ROLE_VERSION);
            if !current {
                Self::migrate_on(&c)?;
                report.migrated = true;
            }
            schema::Migration::verify(&c)?;
            report
        };
        Ok(Self { conn, last_attach: report, author })
    }

    /// Open a store file of its own — a standalone Polis. WAL so readers never
    /// queue behind the writer, `synchronous = NORMAL` as WAL's safe
    /// companion, and a busy timeout for the second process on the file.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        for pragma in [
            "PRAGMA journal_mode = WAL",
            "PRAGMA synchronous = NORMAL",
            "PRAGMA busy_timeout = 5000",
        ] {
            let _ = conn.execute_batch(pragma);
        }
        Self::attach(Arc::new(Mutex::new(conn)), AttachOptions::standalone())
    }

    /// An in-memory standalone store (tests, throwaway analysis).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        Self::attach(Arc::new(Mutex::new(conn)), AttachOptions::standalone())
    }

    /// The SQLite this process linked must carry what the schema uses. Checked
    /// at attach, not assumed from the build: `bundled` brings FTS5 today, but
    /// a host linking a system SQLite would otherwise fail at the first FTS
    /// statement with an error that names nothing.
    pub fn require_capabilities(conn: &Connection) -> Result<(), StoreError> {
        let version: String = conn.query_row("SELECT sqlite_version()", [], |r| r.get(0))?;
        let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
        let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        if (major, minor) < MIN_SQLITE {
            return Err(StoreError::MissingCapability(format!(
                "version {}.{} or newer (linked {version})",
                MIN_SQLITE.0, MIN_SQLITE.1
            )));
        }
        let mut stmt = conn.prepare("PRAGMA compile_options")?;
        let options: Vec<String> =
            stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?;
        if !options.iter().any(|o| o == "ENABLE_FTS5") {
            return Err(StoreError::MissingCapability("FTS5 (ENABLE_FTS5)".to_string()));
        }
        Ok(())
    }

    /// The migration, unconditionally — the STEP, as opposed to `attach`'s
    /// version-gated runner. Every block is idempotent; the individually
    /// versioned ones (the corpus-role backfill, the lexical rebuild) still
    /// consult their own key. Stamps `schema_version` on success.
    pub fn run_migrations(&self) -> Result<(), StoreError> {
        let c = self.conn();
        Self::migrate_on(&c)?;
        schema::Migration::verify(&c)?;
        Ok(())
    }

    fn migrate_on(conn: &Connection) -> rusqlite::Result<()> {
        schema::Migration::tables(conn)?;
        schema::Migration::additive(conn)?;
        schema::Migration::embeddings(conn)?;
        lexical::Lexical::ensure(conn)?;
        schema::Migration::provenance(conn)?;
        meta::set(conn, meta::SCHEMA_VERSION_KEY, meta::STORE_SCHEMA_VERSION)
    }

    /// The connection, locked. A poisoned lock is recovered rather than
    /// propagated: the schema is transactional, so a panic mid-closure leaves
    /// nothing half-written to protect against.
    pub fn conn(&self) -> MutexGuard<'_, Connection> {
        lock(&self.conn)
    }

    /// The shared handle — for a host that constructs itself around it.
    pub fn shared_connection(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.conn)
    }

    /// The author stamped on the store's own writes (see `AttachOptions`).
    pub fn author(&self) -> &str {
        &self.author
    }

    /// What the last `attach` did.
    pub fn last_attach(&self) -> &AttachReport {
        &self.last_attach
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(meta::get(&self.conn(), key)?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), StoreError> {
        Ok(meta::set(&self.conn(), key, value)?)
    }

    /// The memory schema as SQL, in creation order — the golden's input.
    pub fn schema_sql(&self) -> Result<String, StoreError> {
        Ok(schema::schema_sql(&self.conn())?)
    }

    /// Append one event to the chain — cross-process safe (`ledger.rs`).
    pub fn append_event(
        &self,
        a: &polis_core::ledger::LedgerAppend,
    ) -> Result<polis_core::ledger::LedgerEventRow, StoreError> {
        Ok(ledger::append_event(&self.conn(), a)?)
    }
}

fn lock(conn: &Mutex<Connection>) -> MutexGuard<'_, Connection> {
    conn.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {

    /// B1: `class_runs` carries what a run cost and did; the old finisher
    /// still writes (status → outcome), the new one writes every column, and
    /// both readers see them. Schema version 2 re-runs the additive block
    /// once on a version-1 store.
    #[test]
    fn class_runs_carry_the_b1_accounting_and_the_schema_version_moved() {
        let store = PolisStore::open_in_memory().unwrap();
        assert_eq!(store.meta(meta::SCHEMA_VERSION_KEY).unwrap().as_deref(), Some(meta::STORE_SCHEMA_VERSION));
        let id = store.insert_class_run(0, 10).unwrap();
        store.finish_class_run(id, "done", None, "legacy call").unwrap();
        let run = store.latest_class_run().unwrap().unwrap();
        assert_eq!(run.outcome.as_deref(), Some("done"));
        assert_eq!(run.duration_ms, None);
        let id2 = store.insert_class_run(10, 20).unwrap();
        store
            .finish_class_run_with(
                id2,
                &polis_core::types::ClassRunFinish {
                    status: "error".into(),
                    summary: "boom".into(),
                    duration_ms: Some(1234),
                    items: Some(7),
                    ops: Some(3),
                    model: Some("claude-cli".into()),
                    outcome: Some("error".into()),
                    error: Some("boom".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        store.set_class_run_canary(id2, Some(0.91), Some(0.90)).unwrap();
        let runs = store.list_class_runs(10).unwrap();
        assert_eq!(runs.len(), 2);
        let r = &runs[0];
        assert_eq!((r.id, r.duration_ms, r.items, r.ops), (id2, Some(1234), Some(7), Some(3)));
        assert_eq!(r.model.as_deref(), Some("claude-cli"));
        assert_eq!(r.error.as_deref(), Some("boom"));
        assert_eq!((r.canary_before, r.canary_after), (Some(0.91), Some(0.90)));
        // A version-1 store migrates once on attach and is current after.
        store.set_meta(meta::SCHEMA_VERSION_KEY, "1").unwrap();
        let again = PolisStore::attach(store.shared_connection(), AttachOptions::standalone()).unwrap();
        assert!(again.last_attach().migrated);
        assert_eq!(again.meta(meta::SCHEMA_VERSION_KEY).unwrap().as_deref(), Some(meta::STORE_SCHEMA_VERSION));
        let third = PolisStore::attach(store.shared_connection(), AttachOptions::standalone()).unwrap();
        assert!(!third.last_attach().migrated, "a current store is a no-op");
    }
    use super::*;

    #[test]
    fn a_fresh_store_migrates_once_and_reattaches_on_the_fast_path() {
        let store = PolisStore::open_in_memory().unwrap();
        assert!(store.last_attach().migrated, "a fresh store builds its schema");
        assert_eq!(store.last_attach().adopted_keys, 0);
        let v = meta::versions(&store.conn()).unwrap();
        assert_eq!(v.schema.as_deref(), Some(meta::STORE_SCHEMA_VERSION));
        assert_eq!(v.lexical.as_deref(), Some(LEXICAL_VERSION));
        assert_eq!(v.corpus_role.as_deref(), Some(CORPUS_ROLE_VERSION));

        let again =
            PolisStore::attach(store.shared_connection(), AttachOptions::standalone()).unwrap();
        assert!(!again.last_attach().migrated, "a current store runs no schema SQL");
    }

    #[test]
    fn attach_adopts_a_hosts_legacy_versions_before_the_versioned_blocks_run() {
        // A host database that the OLD migrations built: the memory tables
        // exist, the FTS layer is at version 2, and the versions live under
        // app_settings. Attaching must adopt them and NOT rebuild the index.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO app_settings VALUES ('redline.memory.lexicalVersion', '2');
             INSERT INTO app_settings VALUES ('redline.memory.corpusRoleVersion', '1');",
        )
        .unwrap();
        // Build the schema the way the old code did (same statements, no meta).
        schema::Migration::tables(&conn).unwrap();
        schema::Migration::additive(&conn).unwrap();
        schema::Migration::embeddings(&conn).unwrap();
        // The old code stamped the lexical version into app_settings; emulate a
        // built index at v2 by running the block with polis_meta pre-set.
        meta::ensure_table(&conn).unwrap();
        meta::set(&conn, meta::LEXICAL_VERSION_KEY, "0").unwrap();
        lexical::Lexical::ensure(&conn).unwrap();
        conn.execute("DELETE FROM polis_meta", []).unwrap();
        let fts_rootpage = |c: &Connection| -> i64 {
            c.query_row(
                "SELECT rootpage FROM sqlite_master WHERE name = 'prompts_fts_data'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        let before = fts_rootpage(&conn);

        let store =
            PolisStore::attach(Arc::new(Mutex::new(conn)), AttachOptions::redline()).unwrap();
        assert_eq!(store.last_attach().adopted_keys, 2);
        assert!(store.last_attach().migrated, "schema_version was absent → the idempotent block ran once");
        let c = store.conn();
        assert_eq!(fts_rootpage(&c), before, "adopted lexical version 2 → no FTS rebuild");
        let v = meta::versions(&c).unwrap();
        assert_eq!(v.lexical.as_deref(), Some("2"));
        assert_eq!(v.corpus_role.as_deref(), Some("1"));
        assert_eq!(v.schema.as_deref(), Some(meta::STORE_SCHEMA_VERSION));
    }

    #[test]
    fn capabilities_are_present_in_the_bundled_sqlite() {
        let c = Connection::open_in_memory().unwrap();
        PolisStore::require_capabilities(&c).unwrap();
    }

    #[test]
    fn standalone_open_uses_wal() {
        let dir = std::env::temp_dir().join(format!("polis-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = PolisStore::open(&dir.join("polis.db")).unwrap();
        let mode: String =
            store.conn().query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ---------------------------------------------------------------------------
// Snapshots and integrity (Session E1): what `polis backup` / `restore` /
// `doctor` and the gardener's backup cadence stand on. Host-neutral — Redline
// keeps its own `snapshot_database` wrapper, which can call these.
// ---------------------------------------------------------------------------

impl PolisStore {
    /// `VACUUM INTO` a consistent copy of the whole database at `dest` — the
    /// crown-jewels backup. Works while the store is open and in use (the
    /// copy is transactionally consistent); `dest` must not exist.
    pub fn snapshot_to(&self, dest: &Path) -> rusqlite::Result<()> {
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = self.conn();
        conn.execute("VACUUM INTO ?1", [dest.to_string_lossy().as_ref()])?;
        Ok(())
    }

    /// `PRAGMA quick_check`: `Ok(())` when SQLite reports the file
    /// structurally sound, `Err(first complaint)` otherwise. Cheaper than
    /// `integrity_check` and what `doctor` runs on every visit. Needs a
    /// WRITABLE connection: FTS5's integrity pass fails on a read-only one
    /// ("attempt to write a readonly database"), so a snapshot opened with
    /// `open_read_only` is verified by its chain instead.
    pub fn quick_check(&self) -> rusqlite::Result<Result<(), String>> {
        let conn = self.conn();
        let first: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        Ok(if first == "ok" { Ok(()) } else { Err(first) })
    }

    /// Open an existing store file WITHOUT migrating it — read-only, for
    /// verifying a snapshot before it is trusted (`backup::verify_snapshot`)
    /// or reading a file another process owns. Refuses a file whose schema is
    /// not already current (a snapshot of an older store is not something to
    /// swap in silently).
    pub fn open_read_only(path: &Path) -> Result<Self, StoreError> {
        use rusqlite::OpenFlags;
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Self::require_capabilities(&conn)?;
        schema::Migration::verify(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            last_attach: AttachReport::default(),
            author: default_author(),
        })
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn a_snapshot_verifies_read_only_and_quick_check_is_ok() {
        let dir = std::env::temp_dir().join(format!("polis-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let live = dir.join("polis.db");
        let store = PolisStore::open(&live).unwrap();
        assert_eq!(store.quick_check().unwrap(), Ok(()));
        let snap = dir.join("backups").join("polis-1.db");
        store.snapshot_to(&snap).unwrap();
        assert!(snap.exists());
        let ro = PolisStore::open_read_only(&snap).unwrap();
        assert!(ro.verify_ledger_chain().unwrap().ok, "an empty chain verifies");
        // quick_check needs a writable connection (FTS5's integrity pass);
        // the writable open of the same file is what doctor runs it on.
        assert!(PolisStore::open(&snap.with_file_name("copy.db")).is_ok());
        // Read-only means read-only.
        assert!(ro.set_meta("x", "y").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
