use core::{Alert, FileMeta, Snapshot};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;
use uuid::Uuid;

const SCHEMA_VERSION: i64 = 2;

pub struct Db {
    conn: Connection,
}

/// A baseline's identity and the set of roots it watches.
pub struct Baseline {
    pub id: Uuid,
    pub name: String,
    pub roots: Vec<String>,
}

/// Summary row for `ls`.
pub struct BaselineInfo {
    pub id: Uuid,
    pub name: String,
    pub roots: Vec<String>,
    pub file_count: i64,
    pub created_at: i64,
    pub checked_at: Option<i64>,
}

impl Db {
    pub fn open<P: AsRef<Path>>(path: P) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;

        // WAL gives us a single-writer/concurrent-reader journal; NORMAL
        // sync is durable enough for a local tool and much faster than FULL.
        // foreign_keys is per-connection and off by default — without it the
        // ON DELETE CASCADE clauses are silently ignored. It must be set
        // outside a transaction, so it lives here before `migrate` runs.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;

        let mut db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    /// Apply any outstanding migrations, one version at a time. Each step
    /// runs in its own transaction together with the `user_version` bump, so
    /// a failure rolls back cleanly and never leaves a half-applied schema.
    fn migrate(&mut self) -> rusqlite::Result<()> {
        let mut version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?;

        while version < SCHEMA_VERSION {
            let tx = self.conn.transaction()?;
            match version {
                0 => Self::migrate_0_to_1(&tx)?,
                1 => Self::migrate_1_to_2(&tx)?,
                _ => unreachable!("no migration step from v{version}"),
            }
            version += 1;
            tx.execute_batch(&format!("PRAGMA user_version = {version};"))?;
            tx.commit()?;
        }

        Ok(())
    }

    /// v1: the original single-snapshot schema. Left exactly as it shipped —
    /// never edit an applied migration; add the next step instead.
    fn migrate_0_to_1(tx: &rusqlite::Transaction) -> rusqlite::Result<()> {
        tx.execute_batch(
            "CREATE TABLE files (
                 path     TEXT PRIMARY KEY,
                 size     INTEGER NOT NULL,
                 modified INTEGER NOT NULL,
                 hash     TEXT
             );",
        )
    }

    /// v2: multi-baseline schema. The v1 `files` table has no baseline
    /// identity to carry forward, so it is dropped and rebuilt; baselines
    /// regenerate cheaply via a fresh scan.
    fn migrate_1_to_2(tx: &rusqlite::Transaction) -> rusqlite::Result<()> {
        tx.execute_batch(
            "DROP TABLE IF EXISTS files;

             CREATE TABLE baselines (
                 id BLOB PRIMARY KEY,
                 name TEXT NOT NULL UNIQUE,
                 created_at INTEGER NOT NULL,
                 checked_at INTEGER
             );

             CREATE TABLE roots (
                 id INTEGER PRIMARY KEY,
                 path TEXT NOT NULL UNIQUE
             );

             CREATE TABLE baseline_roots (
                 baseline_id BLOB NOT NULL REFERENCES baselines(id) ON DELETE CASCADE,
                 root_id INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
                 PRIMARY KEY (baseline_id, root_id)
             ) WITHOUT ROWID;

             CREATE TABLE files (
                 baseline_id BLOB NOT NULL REFERENCES baselines(id) ON DELETE CASCADE,
                 path TEXT NOT NULL,
                 size INTEGER NOT NULL,
                 modified INTEGER NOT NULL,
                 hash BLOB,
                 PRIMARY KEY (baseline_id, path)
             ) WITHOUT ROWID;

             CREATE TABLE checks (
                 id INTEGER PRIMARY KEY,
                 baseline_id BLOB NOT NULL REFERENCES baselines(id) ON DELETE CASCADE,
                 ran_at INTEGER NOT NULL,
                 files_scanned INTEGER NOT NULL,
                 alert_count INTEGER NOT NULL
             );

             CREATE TABLE alerts (
                 check_id INTEGER NOT NULL REFERENCES checks(id) ON DELETE CASCADE,
                 path TEXT NOT NULL,
                 kind INTEGER NOT NULL,
                 old_size INTEGER,
                 new_size INTEGER,
                 old_hash BLOB,
                 new_hash BLOB
             );

             CREATE INDEX alerts_by_check ON alerts(check_id);
            ",
        )
    }

    /// Look up the baseline named `name`, creating it if absent, and ensure
    /// `root` is registered and linked to it. Returns the baseline's id.
    ///
    /// Calling it again with the same name and a new root adds that root to
    /// the baseline (a baseline watches a *set* of roots).
    pub fn upsert_baseline(&mut self, name: &str, root: &str) -> rusqlite::Result<Uuid> {
        let tx = self.conn.transaction()?;

        // Find or create the baseline by name.
        let id: Uuid = match tx
            .query_row(
                "SELECT id FROM baselines WHERE name = ?1",
                params![name],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .ok()
        {
            Some(bytes) => Uuid::from_slice(&bytes).expect("stored baseline id is a valid uuid"),
            None => {
                let id = Uuid::new_v4();
                tx.execute(
                    "INSERT INTO baselines (id, name, created_at, checked_at)
                     VALUES (?1, ?2, ?3, NULL)",
                    params![id.as_bytes().as_slice(), name, now_nanos()],
                )?;
                id
            }
        };

        // Register the root (idempotent) and fetch its id.
        tx.execute(
            "INSERT OR IGNORE INTO roots (path) VALUES (?1)",
            params![root],
        )?;
        let root_id: i64 = tx.query_row(
            "SELECT id FROM roots WHERE path = ?1",
            params![root],
            |row| row.get(0),
        )?;

        // Link baseline <-> root (idempotent).
        tx.execute(
            "INSERT OR IGNORE INTO baseline_roots (baseline_id, root_id) VALUES (?1, ?2)",
            params![id.as_bytes().as_slice(), root_id],
        )?;

        tx.commit()?;
        Ok(id)
    }

    /// Replace a single baseline's file set with `snapshot`, atomically.
    /// The baseline must already exist (see [`Db::upsert_baseline`]).
    pub fn save(&mut self, baseline_id: &[u8], snapshot: &Snapshot) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM files WHERE baseline_id = ?1",
            params![baseline_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO files (baseline_id, path, size, modified, hash)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (path, meta) in snapshot {
                stmt.execute(params![
                    baseline_id,
                    path,
                    meta.size as i64,
                    meta.modified,
                    meta.hash.as_ref().map(|h| h.as_slice()),
                ])?;
            }
        }
        tx.commit()
    }

    /// Load a single baseline's file set into an in-memory [`Snapshot`].
    pub fn load(&self, baseline_id: &[u8]) -> rusqlite::Result<Snapshot> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, size, modified, hash FROM files WHERE baseline_id = ?1")?;
        let rows = stmt.query_map(params![baseline_id], |row| {
            let hash = match row.get::<_, Option<Vec<u8>>>(3)? {
                Some(bytes) => Some(<[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Blob,
                        "stored hash is not 32 bytes".into(),
                    )
                })?),
                None => None,
            };
            Ok((
                row.get::<_, String>(0)?,
                FileMeta {
                    size: row.get::<_, i64>(1)? as u64,
                    modified: row.get(2)?,
                    hash,
                },
            ))
        })?;

        let mut snapshot: Snapshot = BTreeMap::new();
        for row in rows {
            let (path, meta) = row?;
            snapshot.insert(path, meta);
        }
        Ok(snapshot)
    }

    /// Record one check run against `baseline_id`: a `checks` row plus an
    /// `alerts` row per detected change, and bump the baseline's
    /// `checked_at`. Returns the new check's id. All in one transaction.
    pub fn record_check(
        &mut self,
        baseline_id: &[u8],
        files_scanned: usize,
        old: &Snapshot,
        new: &Snapshot,
        diff: &BTreeMap<String, Alert>,
    ) -> rusqlite::Result<i64> {
        let now = now_nanos();
        let tx = self.conn.transaction()?;

        tx.execute(
            "INSERT INTO checks (baseline_id, ran_at, files_scanned, alert_count)
             VALUES (?1, ?2, ?3, ?4)",
            params![baseline_id, now, files_scanned as i64, diff.len() as i64],
        )?;
        let check_id = tx.last_insert_rowid();

        tx.execute(
            "UPDATE baselines SET checked_at = ?1 WHERE id = ?2",
            params![now, baseline_id],
        )?;

        {
            let mut stmt = tx.prepare(
                "INSERT INTO alerts
                     (check_id, path, kind, old_size, new_size, old_hash, new_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (path, alert) in diff {
                // kind: 0=added 1=modified 2=deleted; old/new meta depend on it.
                let (kind, old_meta, new_meta) = match alert {
                    Alert::Added => (0, None, new.get(path)),
                    Alert::Modified => (1, old.get(path), new.get(path)),
                    Alert::Deleted => (2, old.get(path), None),
                };
                let old_hash = old_meta.and_then(|m| m.hash);
                let new_hash = new_meta.and_then(|m| m.hash);
                stmt.execute(params![
                    check_id,
                    path,
                    kind,
                    old_meta.map(|m| m.size as i64),
                    new_meta.map(|m| m.size as i64),
                    old_hash.as_ref().map(|h| h.as_slice()),
                    new_hash.as_ref().map(|h| h.as_slice()),
                ])?;
            }
        }

        tx.commit()?;
        Ok(check_id)
    }

    /// Look up a baseline by its unique name.
    pub fn baseline_by_name(&self, name: &str) -> rusqlite::Result<Option<Baseline>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, name FROM baselines WHERE name = ?1",
                params![name],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        row.map(|(id, name)| self.baseline_from_row(id, name)).transpose()
    }

    /// Look up a baseline by its uuid (16 raw bytes).
    pub fn baseline_by_id(&self, id: &[u8]) -> rusqlite::Result<Option<Baseline>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, name FROM baselines WHERE id = ?1",
                params![id],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        row.map(|(id, name)| self.baseline_from_row(id, name)).transpose()
    }

    /// Build a [`Baseline`] from an id/name row, fetching its roots.
    fn baseline_from_row(&self, id_bytes: Vec<u8>, name: String) -> rusqlite::Result<Baseline> {
        let roots = self.roots_for(&id_bytes)?;
        Ok(Baseline {
            id: uuid_from(&id_bytes),
            name,
            roots,
        })
    }

    /// The canonical root paths a baseline watches.
    fn roots_for(&self, baseline_id: &[u8]) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.path
               FROM roots r
               JOIN baseline_roots br ON br.root_id = r.id
              WHERE br.baseline_id = ?1
              ORDER BY r.path",
        )?;
        let rows = stmt.query_map(params![baseline_id], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    /// All baselines with a quick summary, ordered by name.
    pub fn list_baselines(&self) -> rusqlite::Result<Vec<BaselineInfo>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, created_at, checked_at FROM baselines ORDER BY name")?;
        let base: Vec<(Vec<u8>, String, i64, Option<i64>)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;

        let mut out = Vec::with_capacity(base.len());
        for (id_bytes, name, created_at, checked_at) in base {
            let file_count: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM files WHERE baseline_id = ?1",
                params![id_bytes.as_slice()],
                |row| row.get(0),
            )?;
            let roots = self.roots_for(&id_bytes)?;
            out.push(BaselineInfo {
                id: uuid_from(&id_bytes),
                name,
                roots,
                file_count,
                created_at,
                checked_at,
            });
        }
        Ok(out)
    }

    /// Delete a baseline and everything hanging off it (files, checks,
    /// alerts, root links cascade). Shared `roots` rows are left in place.
    /// Returns the number of baseline rows removed (0 or 1).
    pub fn remove_baseline(&mut self, baseline_id: &[u8]) -> rusqlite::Result<usize> {
        self.conn
            .execute("DELETE FROM baselines WHERE id = ?1", params![baseline_id])
    }
}

/// Current wall-clock time as nanoseconds since the UNIX epoch.
fn now_nanos() -> i64 {
    core::system_time_to_nanos(SystemTime::now())
}

/// Reconstruct a [`Uuid`] from the raw bytes stored in the DB. The column is
/// always written from `Uuid::as_bytes`, so a bad length is a corrupt DB.
fn uuid_from(bytes: &[u8]) -> Uuid {
    Uuid::from_slice(bytes).expect("baseline id in DB is a valid 16-byte uuid")
}
