use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::Connection;

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE settings (
  name  TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- Times in milliseconds, since a command and its reply often land in the same
-- second.
CREATE TABLE activity (
  id      INTEGER PRIMARY KEY,
  at      INTEGER NOT NULL,
  kind    TEXT NOT NULL,
  summary TEXT NOT NULL,
  payload TEXT NOT NULL DEFAULT '',
  acked   INTEGER,
  error   TEXT NOT NULL DEFAULT ''
);

CREATE TABLE deadlines (
  name TEXT PRIMARY KEY,
  at   INTEGER NOT NULL
);

CREATE TABLE frames (
  id   INTEGER PRIMARY KEY,
  ts   INTEGER NOT NULL,
  jpeg BLOB NOT NULL
);
CREATE INDEX frames_ts ON frames(ts);

CREATE TABLE jobs (
  id       INTEGER PRIMARY KEY,
  name     TEXT NOT NULL,
  start_ts INTEGER NOT NULL,
  end_ts   INTEGER
);

-- kinds is the comma-separated notifications the device chose; NULL means it
-- has never chosen and gets every kind.
CREATE TABLE subscriptions (
  id              INTEGER PRIMARY KEY,
  endpoint        TEXT NOT NULL UNIQUE,
  p256dh          BLOB NOT NULL,
  auth            BLOB NOT NULL,
  created_ts      INTEGER NOT NULL,
  kinds           TEXT,
  bed_interval    INTEGER NOT NULL DEFAULT 0,
  bed_reminded_ts INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE server_key (
  id  INTEGER PRIMARY KEY CHECK (id = 1),
  der BLOB NOT NULL
);

CREATE TABLE sessions (
  id      TEXT PRIMARY KEY,
  subject TEXT NOT NULL,
  name    TEXT NOT NULL DEFAULT '',
  created INTEGER NOT NULL,
  expires INTEGER NOT NULL
);
CREATE INDEX sessions_expires ON sessions(expires);

CREATE TABLE pending_logins (
  state    TEXT PRIMARY KEY,
  verifier TEXT NOT NULL,
  nonce    TEXT NOT NULL,
  next     TEXT NOT NULL DEFAULT '',
  expires  INTEGER NOT NULL
);
CREATE INDEX pending_logins_expires ON pending_logins(expires);
"#,
    r#"
-- Several printers, each named. The one printer of version 1, if it was set
-- up, becomes printer 1 and keeps its footage, jobs, events and timers.
CREATE TABLE printers (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  name        TEXT NOT NULL,
  ip          TEXT NOT NULL,
  serial      TEXT NOT NULL,
  access_code TEXT NOT NULL
);
INSERT INTO printers (id, name, ip, serial, access_code)
  SELECT 1, 'P1S', ip.value, serial.value, code.value
  FROM settings ip, settings serial, settings code
  WHERE ip.name = 'printer-ip' AND serial.name = 'printer-serial'
    AND code.name = 'printer-access-code';
DELETE FROM settings WHERE name IN ('printer-ip', 'printer-serial',
  'printer-access-code', 'retention', 'kept-jobs', 'database-limit');

ALTER TABLE frames ADD COLUMN printer_id INTEGER NOT NULL DEFAULT 0;
ALTER TABLE jobs ADD COLUMN printer_id INTEGER NOT NULL DEFAULT 0;
ALTER TABLE activity ADD COLUMN printer_id INTEGER;
UPDATE frames SET printer_id = 1;
UPDATE jobs SET printer_id = 1;
UPDATE activity SET printer_id = 1 WHERE kind IN ('command', 'report');
DELETE FROM frames WHERE NOT EXISTS (SELECT 1 FROM printers);
DELETE FROM jobs WHERE NOT EXISTS (SELECT 1 FROM printers);
DROP INDEX frames_ts;
CREATE INDEX frames_printer_ts ON frames(printer_id, ts);
CREATE INDEX jobs_printer ON jobs(printer_id, start_ts);

-- Timers are named "<printer id>:<timer>".
UPDATE deadlines SET name = '1:' || name WHERE EXISTS (SELECT 1 FROM printers);
DELETE FROM deadlines WHERE name NOT LIKE '%:%';

-- When each device was last reminded about each printer's bed.
CREATE TABLE bed_reminders (
  endpoint   TEXT NOT NULL,
  printer_id INTEGER NOT NULL,
  at         INTEGER NOT NULL,
  PRIMARY KEY (endpoint, printer_id)
);
INSERT INTO bed_reminders (endpoint, printer_id, at)
  SELECT endpoint, 1, bed_reminded_ts FROM subscriptions
  WHERE bed_reminded_ts > 0 AND EXISTS (SELECT 1 FROM printers);
ALTER TABLE subscriptions DROP COLUMN bed_reminded_ts;
"#,
    r#"
-- The printer's own timelapses on its SD card replace the recorded footage.
DROP TABLE frames;
DROP TABLE jobs;
DELETE FROM settings WHERE name = 'camera-storage';
"#,
];

const RECLAIM_CHUNK: i64 = 256;

impl Db {
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Db> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        migrate(&conn)?;
        Ok(Db(Arc::new(Mutex::new(conn))))
    }

    #[cfg(test)]
    pub fn memory() -> Db {
        Db::open(":memory:").expect("open in-memory database")
    }

    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn reclaim(&self) -> rusqlite::Result<()> {
        loop {
            let conn = self.lock();
            let free: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
            if free == 0 {
                return Ok(());
            }
            conn.execute_batch(&format!("PRAGMA incremental_vacuum({RECLAIM_CHUNK})"))?;
        }
    }

    #[cfg(test)]
    fn file_size(&self) -> i64 {
        let conn = self.lock();
        let pages: i64 = conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        let size: i64 = conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .unwrap();
        pages * size
    }
}

fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let applied: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(applied as usize) {
        conn.execute_batch(&format!(
            "BEGIN; {sql}; PRAGMA user_version = {}; COMMIT;",
            i + 1
        ))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_database_is_incremental_and_current() {
        let db = Db::memory();
        let conn = db.lock();
        let mode: i64 = conn
            .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, 2);
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version as usize, MIGRATIONS.len());
    }

    fn version_one(sql: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!("{}; PRAGMA user_version = 1;", MIGRATIONS[0]))
            .unwrap();
        conn.execute_batch(sql).unwrap();
        conn.execute_batch(&format!("{}; PRAGMA user_version = 2;", MIGRATIONS[1]))
            .unwrap();
        conn
    }

    const OLD_DATA: &str = "
        INSERT INTO frames (ts, jpeg) VALUES (1, x'00');
        INSERT INTO jobs (name, start_ts) VALUES ('a', 1);
        INSERT INTO activity (at, kind, summary) VALUES (1, 'report', 'r'), (2, 'notification', 'n');
        INSERT INTO deadlines VALUES ('bed-off', 5);
        INSERT INTO subscriptions (endpoint, p256dh, auth, created_ts, bed_reminded_ts)
          VALUES ('e', x'00', x'00', 1, 9);";

    fn all(conn: &Connection, sql: &str) -> Vec<String> {
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn version_two_makes_the_one_printer_printer_one() {
        let conn = version_one(&format!(
            "INSERT INTO settings VALUES ('printer-ip', '10.0.0.5'), ('printer-serial', 'S1'),
               ('printer-access-code', 'c'), ('retention', '3600'), ('bed-off-after', '7200');
             {OLD_DATA}"
        ));
        assert_eq!(
            all(
                &conn,
                "SELECT id || ' ' || name || ' ' || ip || ' ' || serial || ' ' || access_code FROM printers"
            ),
            ["1 P1S 10.0.0.5 S1 c"]
        );
        assert_eq!(all(&conn, "SELECT name FROM settings"), ["bed-off-after"]);
        assert_eq!(all(&conn, "SELECT printer_id || '' FROM frames"), ["1"]);
        assert_eq!(all(&conn, "SELECT printer_id || '' FROM jobs"), ["1"]);
        assert_eq!(
            all(
                &conn,
                "SELECT kind || ' ' || COALESCE(printer_id, '-') FROM activity ORDER BY id"
            ),
            ["report 1", "notification -"]
        );
        assert_eq!(all(&conn, "SELECT name FROM deadlines"), ["1:bed-off"]);
        assert_eq!(
            all(
                &conn,
                "SELECT endpoint || ' ' || printer_id || ' ' || at FROM bed_reminders"
            ),
            ["e 1 9"]
        );
    }

    #[test]
    fn version_two_without_a_printer_drops_what_belonged_to_none() {
        let conn = version_one(OLD_DATA);
        assert!(all(&conn, "SELECT name FROM printers").is_empty());
        assert!(all(&conn, "SELECT ts || '' FROM frames").is_empty());
        assert!(all(&conn, "SELECT name FROM jobs").is_empty());
        assert!(all(&conn, "SELECT name FROM deadlines").is_empty());
        assert!(all(&conn, "SELECT endpoint FROM bed_reminders").is_empty());
    }

    #[test]
    fn version_three_drops_the_footage_and_its_setting() {
        let conn = version_one(&format!(
            "INSERT INTO settings VALUES ('printer-ip', '10.0.0.5'), ('printer-serial', 'S1'),
               ('printer-access-code', 'c'), ('camera-storage', '2048'), ('bed-off-after', '7200');
             {OLD_DATA}"
        ));
        migrate(&conn).unwrap();
        assert_eq!(all(&conn, "SELECT name FROM settings"), ["bed-off-after"]);
        assert!(
            all(
                &conn,
                "SELECT name FROM sqlite_master WHERE name IN ('frames', 'jobs')"
            )
            .is_empty()
        );
    }

    #[test]
    fn deleting_alone_does_not_shrink_the_file_and_reclaiming_does() {
        let db = Db::open(crate::testing::tempdir().join("reclaim.sqlite")).unwrap();
        for i in 0..400 {
            db.lock()
                .execute(
                    "INSERT INTO activity (at, kind, summary) VALUES (?, 'report', ?)",
                    rusqlite::params![i, "x".repeat(8000)],
                )
                .unwrap();
        }
        let before = db.file_size();
        db.lock().execute("DELETE FROM activity", []).unwrap();
        assert_eq!(db.file_size(), before);
        db.reclaim().unwrap();
        assert!(db.file_size() < before / 10);
    }

    #[test]
    fn reopening_keeps_data_and_skips_migrations() {
        let dir = crate::testing::tempdir();
        let path = dir.join("test.sqlite");
        {
            let db = Db::open(&path).unwrap();
            db.lock()
                .execute("INSERT INTO settings VALUES ('a', 'b')", [])
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let value: String = db
            .lock()
            .query_row("SELECT value FROM settings WHERE name = 'a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(value, "b");
        let mode: String = db
            .lock()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }
}
