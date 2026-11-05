//! SQLite persistence.
//!
//! One connection guarded by a mutex: every statement is tiny (single-row
//! updates, a periodic batched checkpoint), so contention is negligible and we
//! avoid a connection pool. WAL + `synchronous=NORMAL` keeps writes cheap while
//! staying consistent after a crash; checkpoints of resume state use explicit
//! transactions so a power loss leaves either the old or the new state.

use crate::model::*;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::{Path, PathBuf};

pub type DbResult<T> = Result<T, rusqlite::Error>;

const SCHEMA_VERSION: i64 = 5;

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> DbResult<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Writes the WAL back into the main database file (on shutdown).
    pub fn checkpoint(&self) {
        let conn = self.conn.lock();
        if let Err(e) = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
            tracing::warn!("database checkpoint failed: {e}");
        }
    }

    pub fn open_in_memory() -> DbResult<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> DbResult<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;",
        )?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> DbResult<()> {
        let conn = self.conn.lock();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;
        let version: i64 = conn
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if version < 1 {
            conn.execute_batch(
                "BEGIN;
                CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE categories (
                    id TEXT PRIMARY KEY, name TEXT NOT NULL, icon TEXT NOT NULL,
                    extensions TEXT NOT NULL, save_dir TEXT, builtin INTEGER NOT NULL,
                    position INTEGER NOT NULL);
                CREATE TABLE queues (
                    id TEXT PRIMARY KEY, name TEXT NOT NULL, max_concurrent INTEGER NOT NULL,
                    speed_limit INTEGER NOT NULL, position INTEGER NOT NULL, builtin INTEGER NOT NULL,
                    schedule TEXT NOT NULL, running INTEGER NOT NULL DEFAULT 0);
                CREATE TABLE downloads (
                    id TEXT PRIMARY KEY, kind TEXT NOT NULL, url TEXT NOT NULL, final_url TEXT,
                    file_name TEXT NOT NULL, save_dir TEXT NOT NULL, size INTEGER,
                    downloaded INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL, error TEXT,
                    category TEXT NOT NULL, queue_id TEXT, description TEXT NOT NULL DEFAULT '',
                    referer TEXT, added_at INTEGER NOT NULL, last_try_at INTEGER, finished_at INTEGER,
                    resumable INTEGER, connections INTEGER NOT NULL, speed_limit INTEGER NOT NULL DEFAULT 0,
                    checksum TEXT, trashed INTEGER NOT NULL DEFAULT 0, position INTEGER NOT NULL DEFAULT 0,
                    mirrors TEXT NOT NULL DEFAULT '[]', grabber_project TEXT,
                    request TEXT, resume TEXT, media TEXT);
                CREATE INDEX downloads_status ON downloads(status);
                CREATE INDEX downloads_queue ON downloads(queue_id, position);
                CREATE TABLE grabber_projects (
                    id TEXT PRIMARY KEY, name TEXT NOT NULL, config TEXT NOT NULL,
                    results TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
                COMMIT;",
            )?;
        }
        if version < 2 {
            // Auto-extract status for finished downloads (v1 tables lack it).
            conn.execute_batch(
                "ALTER TABLE downloads ADD COLUMN extracted_dir TEXT;
                 ALTER TABLE downloads ADD COLUMN extract_error TEXT;",
            )?;
        }
        if version < 3 {
            // LAN sharing flag for finished downloads (v2 tables lack it).
            conn.execute_batch(
                "ALTER TABLE downloads ADD COLUMN lan_shared INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        if version < 4 {
            // Subtitle-translation note for finished videos (v3 tables lack it).
            conn.execute_batch(
                "ALTER TABLE downloads ADD COLUMN subtitle_note TEXT;",
            )?;
        }
        if version < 5 {
            // Dead-link check outcome for queued downloads (v4 tables lack it).
            conn.execute_batch(
                "ALTER TABLE downloads ADD COLUMN link_status TEXT;
                 ALTER TABLE downloads ADD COLUMN link_checked_at INTEGER;",
            )?;
        }
        conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    // ----- settings -------------------------------------------------------------------------

    pub fn get_setting(&self, key: &str) -> DbResult<Option<String>> {
        self.conn
            .lock()
            .query_row(
                "SELECT value FROM settings WHERE key=?1",
                params![key],
                |r| r.get(0),
            )
            .optional()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> DbResult<()> {
        self.conn.lock().execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ----- categories -----------------------------------------------------------------------

    pub fn categories(&self) -> DbResult<Vec<Category>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, icon, extensions, save_dir, builtin, position FROM categories ORDER BY position",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Category {
                id: r.get(0)?,
                name: r.get(1)?,
                icon: r.get(2)?,
                extensions: json_col(r, 3)?,
                save_dir: r.get::<_, Option<String>>(4)?.map(PathBuf::from),
                builtin: r.get(5)?,
                position: r.get(6)?,
            })
        })?;
        rows.collect()
    }

    pub fn upsert_category(&self, c: &Category) -> DbResult<()> {
        self.conn.lock().execute(
            "INSERT INTO categories(id, name, icon, extensions, save_dir, builtin, position)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET name=excluded.name, icon=excluded.icon,
               extensions=excluded.extensions, save_dir=excluded.save_dir, position=excluded.position",
            params![
                c.id,
                c.name,
                c.icon,
                to_json(&c.extensions),
                c.save_dir.as_ref().map(|p| p.to_string_lossy().into_owned()),
                c.builtin,
                c.position
            ],
        )?;
        Ok(())
    }

    pub fn delete_category(&self, id: &str) -> DbResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE downloads SET category='other' WHERE category=?1",
            params![id],
        )?;
        conn.execute(
            "DELETE FROM categories WHERE id=?1 AND builtin=0",
            params![id],
        )?;
        Ok(())
    }

    // ----- queues ---------------------------------------------------------------------------

    pub fn queues(&self) -> DbResult<Vec<Queue>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, max_concurrent, speed_limit, position, builtin, schedule, running
             FROM queues ORDER BY position",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Queue {
                id: r.get(0)?,
                name: r.get(1)?,
                max_concurrent: r.get(2)?,
                speed_limit: r.get::<_, i64>(3)? as u64,
                position: r.get(4)?,
                builtin: r.get(5)?,
                schedule: json_col(r, 6)?,
                running: r.get(7)?,
            })
        })?;
        rows.collect()
    }

    pub fn upsert_queue(&self, q: &Queue) -> DbResult<()> {
        self.conn.lock().execute(
            "INSERT INTO queues(id, name, max_concurrent, speed_limit, position, builtin, schedule, running)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET name=excluded.name, max_concurrent=excluded.max_concurrent,
               speed_limit=excluded.speed_limit, position=excluded.position, schedule=excluded.schedule,
               running=excluded.running",
            params![
                q.id,
                q.name,
                q.max_concurrent,
                q.speed_limit as i64,
                q.position,
                q.builtin,
                to_json(&q.schedule),
                q.running
            ],
        )?;
        Ok(())
    }

    pub fn delete_queue(&self, id: &str) -> DbResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE downloads SET queue_id=NULL WHERE queue_id=?1",
            params![id],
        )?;
        conn.execute("DELETE FROM queues WHERE id=?1 AND builtin=0", params![id])?;
        Ok(())
    }

    // ----- downloads ------------------------------------------------------------------------

    pub fn downloads(&self) -> DbResult<Vec<DownloadItem>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {DOWNLOAD_COLUMNS} FROM downloads ORDER BY added_at"
        ))?;
        let rows = stmt.query_map([], row_to_item)?;
        rows.collect()
    }

    pub fn download(&self, id: &str) -> DbResult<Option<DownloadItem>> {
        let conn = self.conn.lock();
        conn.query_row(
            &format!("SELECT {DOWNLOAD_COLUMNS} FROM downloads WHERE id=?1"),
            params![id],
            row_to_item,
        )
        .optional()
    }

    pub fn upsert_download(&self, d: &DownloadItem) -> DbResult<()> {
        let conn = self.conn.lock();
        upsert_download_on(&conn, d)
    }

    /// Persist several items atomically (used by the periodic checkpoint).
    pub fn upsert_downloads(&self, items: &[DownloadItem]) -> DbResult<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        for d in items {
            upsert_download_on(&tx, d)?;
        }
        tx.commit()
    }

    pub fn delete_download(&self, id: &str) -> DbResult<()> {
        self.conn
            .lock()
            .execute("DELETE FROM downloads WHERE id=?1", params![id])?;
        Ok(())
    }

    // ----- grabber --------------------------------------------------------------------------

    pub fn grabber_projects(&self) -> DbResult<Vec<GrabberProject>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, config, results, created_at, updated_at FROM grabber_projects ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(GrabberProject {
                id: r.get(0)?,
                name: r.get(1)?,
                config: json_col(r, 2)?,
                results: json_col(r, 3)?,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })?;
        rows.collect()
    }

    pub fn upsert_grabber_project(&self, p: &GrabberProject) -> DbResult<()> {
        self.conn.lock().execute(
            "INSERT INTO grabber_projects(id, name, config, results, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET name=excluded.name, config=excluded.config,
               results=excluded.results, updated_at=excluded.updated_at",
            params![
                p.id,
                p.name,
                p.config.to_string(),
                p.results.to_string(),
                p.created_at,
                p.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn delete_grabber_project(&self, id: &str) -> DbResult<()> {
        self.conn
            .lock()
            .execute("DELETE FROM grabber_projects WHERE id=?1", params![id])?;
        Ok(())
    }
}

const DOWNLOAD_COLUMNS: &str = "id, kind, url, final_url, file_name, save_dir, size, downloaded, status, error,
    category, queue_id, description, referer, added_at, last_try_at, finished_at, resumable, connections,
    speed_limit, checksum, trashed, position, mirrors, grabber_project, request, resume, media,
    extracted_dir, extract_error, lan_shared, subtitle_note, link_status, link_checked_at";

fn upsert_download_on(conn: &Connection, d: &DownloadItem) -> DbResult<()> {
    conn.execute(
        "INSERT INTO downloads(id, kind, url, final_url, file_name, save_dir, size, downloaded, status, error,
            category, queue_id, description, referer, added_at, last_try_at, finished_at, resumable, connections,
            speed_limit, checksum, trashed, position, mirrors, grabber_project, request, resume, media,
            extracted_dir, extract_error, lan_shared, subtitle_note, link_status, link_checked_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31,?32,?33,?34)
         ON CONFLICT(id) DO UPDATE SET kind=excluded.kind, url=excluded.url, final_url=excluded.final_url,
            file_name=excluded.file_name, save_dir=excluded.save_dir, size=excluded.size,
            downloaded=excluded.downloaded, status=excluded.status, error=excluded.error,
            category=excluded.category, queue_id=excluded.queue_id, description=excluded.description,
            referer=excluded.referer, last_try_at=excluded.last_try_at, finished_at=excluded.finished_at,
            resumable=excluded.resumable, connections=excluded.connections, speed_limit=excluded.speed_limit,
            checksum=excluded.checksum, trashed=excluded.trashed, position=excluded.position,
            mirrors=excluded.mirrors, grabber_project=excluded.grabber_project, request=excluded.request,
            resume=excluded.resume, media=excluded.media, extracted_dir=excluded.extracted_dir,
            extract_error=excluded.extract_error, lan_shared=excluded.lan_shared,
            subtitle_note=excluded.subtitle_note, link_status=excluded.link_status,
            link_checked_at=excluded.link_checked_at",
        params![
            d.id,
            d.kind.as_str(),
            d.url,
            d.final_url,
            d.file_name,
            d.save_dir.to_string_lossy(),
            d.size.map(|s| s as i64),
            d.downloaded as i64,
            d.status.as_str(),
            d.error,
            d.category,
            d.queue_id,
            d.description,
            d.referer,
            d.added_at,
            d.last_try_at,
            d.finished_at,
            d.resumable,
            d.connections,
            d.speed_limit as i64,
            d.checksum,
            d.trashed,
            d.position,
            to_json(&d.mirrors),
            d.grabber_project,
            d.request.as_ref().map(|v| v.to_string()),
            d.resume.as_ref().map(|v| v.to_string()),
            d.media.as_ref().map(to_json),
            d.extracted_dir.as_ref().map(|p| p.to_string_lossy().into_owned()),
            d.extract_error,
            d.lan_shared,
            d.subtitle_note,
            d.link_status.map(|s| s.as_str()),
            d.link_checked_at.map(|t| t as i64),
        ],
    )?;
    Ok(())
}

fn row_to_item(r: &Row<'_>) -> DbResult<DownloadItem> {
    Ok(DownloadItem {
        id: r.get(0)?,
        kind: ItemKind::parse(&r.get::<_, String>(1)?),
        url: r.get(2)?,
        final_url: r.get(3)?,
        file_name: r.get(4)?,
        save_dir: PathBuf::from(r.get::<_, String>(5)?),
        size: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
        downloaded: r.get::<_, i64>(7)? as u64,
        status: Status::parse(&r.get::<_, String>(8)?),
        error: r.get(9)?,
        category: r.get(10)?,
        queue_id: r.get(11)?,
        description: r.get(12)?,
        referer: r.get(13)?,
        added_at: r.get(14)?,
        last_try_at: r.get(15)?,
        finished_at: r.get(16)?,
        resumable: r.get(17)?,
        connections: r.get(18)?,
        speed_limit: r.get::<_, i64>(19)? as u64,
        checksum: r.get(20)?,
        trashed: r.get(21)?,
        position: r.get(22)?,
        mirrors: json_col(r, 23)?,
        grabber_project: r.get(24)?,
        request: opt_json_col(r, 25)?,
        resume: opt_json_col(r, 26)?,
        media: opt_json_col(r, 27)?,
        extracted_dir: r.get::<_, Option<String>>(28)?.map(PathBuf::from),
        extract_error: r.get(29)?,
        lan_shared: r.get(30)?,
        subtitle_note: r.get(31)?,
        link_status: r.get::<_, Option<String>>(32)?.map(|s| LinkStatus::parse(&s)),
        link_checked_at: r.get::<_, Option<i64>>(33)?.map(|v| v as u64),
    })
}

fn to_json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into())
}

fn json_col<T: serde::de::DeserializeOwned + Default>(r: &Row<'_>, idx: usize) -> DbResult<T> {
    let s: String = r.get(idx)?;
    Ok(serde_json::from_str(&s).unwrap_or_default())
}

fn opt_json_col<T: serde::de::DeserializeOwned>(r: &Row<'_>, idx: usize) -> DbResult<Option<T>> {
    let s: Option<String> = r.get(idx)?;
    Ok(s.and_then(|s| serde_json::from_str(&s).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let mut item = DownloadItem {
            id: "a".into(),
            url: "https://example.com/f.zip".into(),
            file_name: "f.zip".into(),
            save_dir: "C:/tmp".into(),
            category: "compressed".into(),
            connections: 8,
            mirrors: vec!["https://mirror/f.zip".into()],
            resume: Some(serde_json::json!({"segments": [[0, 10, 20]]})),
            ..Default::default()
        };
        db.upsert_download(&item).unwrap();
        item.status = Status::Paused;
        item.downloaded = 42;
        item.extracted_dir = Some(PathBuf::from("C:/tmp/f"));
        item.extract_error = Some("bad archive".into());
        db.upsert_downloads(std::slice::from_ref(&item)).unwrap();
        let loaded = db.download("a").unwrap().unwrap();
        assert_eq!(loaded.status, Status::Paused);
        assert_eq!(loaded.downloaded, 42);
        assert_eq!(loaded.mirrors, item.mirrors);
        assert_eq!(loaded.resume, item.resume);
        assert_eq!(loaded.extracted_dir, item.extracted_dir);
        assert_eq!(loaded.extract_error, item.extract_error);
        db.delete_download("a").unwrap();
        assert!(db.downloads().unwrap().is_empty());
    }

    #[test]
    fn queues_and_settings() {
        let db = Db::open_in_memory().unwrap();
        let q = Queue {
            id: "main".into(),
            name: "Main queue".into(),
            max_concurrent: 3,
            builtin: true,
            ..Default::default()
        };
        db.upsert_queue(&q).unwrap();
        assert_eq!(db.queues().unwrap()[0].max_concurrent, 3);
        db.set_setting("k", "v1").unwrap();
        db.set_setting("k", "v2").unwrap();
        assert_eq!(db.get_setting("k").unwrap().as_deref(), Some("v2"));
    }
}
