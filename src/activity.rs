//! A persistent event log of commands sent to the printer (and whether they
//! were acknowledged), printer reports, and notifications sent. It is bounded
//! by size in bytes, since entries vary widely in size.

use std::sync::{Arc, Mutex};

use rusqlite::params;
use serde::Serialize;

use crate::capacity::{Item, Source};
use crate::clock::Clock;
use crate::db::Db;

/// What kind of thing an entry records.
pub const COMMAND: &str = "command"; // sent to the printer
pub const REPORT: &str = "report"; // received from the printer
pub const NOTIFICATION: &str = "notification"; // sent to subscribed devices

/// How much of one payload is kept (the first report after connecting is the
/// printer's whole state).
const MAX_PAYLOAD: usize = 4096;

/// Approximates the columns `SIZE` does not measure (id, timestamps, kind), so
/// many tiny entries still count against the budget.
const ROW_OVERHEAD: i64 = 64;

/// How far under the limit a trim cuts, so a full log isn't trimmed on every
/// insert.
const LOW_WATER: f64 = 0.9;

/// What one stored row costs, in bytes.
const SIZE: &str = "octet_length(summary) + octet_length(payload) + octet_length(error) + 64";

/// One logged event, as the Events screen is sent it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    pub id: i64,
    #[serde(serialize_with = "rfc3339")]
    pub at: i64,
    pub kind: String,
    pub summary: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub payload: String,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "rfc3339_opt"
    )]
    pub acked: Option<i64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// The printer it was about, if it was about one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer: Option<i64>,
}

fn to_rfc3339(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, false))
        .unwrap_or_default()
}

fn rfc3339<S: serde::Serializer>(ms: &i64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&to_rfc3339(*ms))
}

fn rfc3339_opt<S: serde::Serializer>(ms: &Option<i64>, s: S) -> Result<S::Ok, S::Error> {
    match ms {
        Some(ms) => rfc3339(ms, s),
        None => s.serialize_none(),
    }
}

/// Records what happened, in the database, bounded by a size in bytes.
#[derive(Clone)]
pub struct Log {
    db: Db,
    limit: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// What the stored entries currently come to. Held while writing, so the
    /// count and the table move together.
    bytes: Arc<Mutex<i64>>,
    clock: Clock,
    /// Which printer this handle records for, if any.
    printer: Option<i64>,
}

impl Log {
    /// `limit` returns the budget in bytes and is read on every write.
    pub fn new(
        db: Db,
        limit: impl Fn() -> i64 + Send + Sync + 'static,
        clock: Clock,
    ) -> rusqlite::Result<Log> {
        // Count once; writes keep the total up to date after that.
        let bytes = total(&db)?;
        Ok(Log {
            db,
            limit: Arc::new(limit),
            bytes: Arc::new(Mutex::new(bytes)),
            clock,
            printer: None,
        })
    }

    /// The same log, with every entry recorded through it marked as being
    /// about printer `id`.
    pub fn for_printer(&self, id: i64) -> Log {
        Log {
            printer: Some(id),
            ..self.clone()
        }
    }

    /// Adds an entry and returns its id, so it can be acknowledged later. A
    /// failed write is logged and returns None; it never blocks the command
    /// being recorded.
    pub fn record(&self, kind: &str, summary: &str, payload: &str) -> Option<i64> {
        let payload = truncate(payload);
        let at = (self.clock)();
        let mut bytes = self.bytes.lock().unwrap_or_else(|p| p.into_inner());
        let conn = self.db.lock();
        let inserted = conn.execute(
            "INSERT INTO activity (at, kind, summary, payload, printer_id) VALUES (?, ?, ?, ?, ?)",
            params![at, kind, summary, payload, self.printer],
        );
        if let Err(err) = inserted {
            tracing::warn!("activity: recording {kind} {summary:?}: {err}");
            return None;
        }
        let id = conn.last_insert_rowid();
        *bytes += (summary.len() + payload.len()) as i64 + ROW_OVERHEAD;
        self.trim(&conn, &mut bytes);
        Some(id)
    }

    /// Records when the printer's broker confirmed a message (`Ok(at_ms)`), or
    /// why it didn't. The row may already have been trimmed, in which case
    /// nothing changes.
    pub fn acknowledge(&self, id: Option<i64>, result: Result<i64, String>) {
        let Some(id) = id else { return };
        let mut bytes = self.bytes.lock().unwrap_or_else(|p| p.into_inner());
        let conn = self.db.lock();
        match result {
            // Fixed-size column, covered by ROW_OVERHEAD.
            Ok(at) => {
                if let Err(err) = conn.execute(
                    "UPDATE activity SET acked = ? WHERE id = ?",
                    params![at, id],
                ) {
                    tracing::warn!("activity: acknowledging {id}: {err}");
                }
            }
            Err(error) => match conn.execute(
                "UPDATE activity SET error = ? WHERE id = ?",
                params![error, id],
            ) {
                // Only count the error if the row still existed.
                Ok(changed) if changed > 0 => {
                    *bytes += error.len() as i64;
                    self.trim(&conn, &mut bytes);
                }
                Ok(_) => {}
                Err(err) => tracing::warn!("activity: acknowledging {id}: {err}"),
            },
        }
    }

    /// Deletes the oldest entries until the log is back under its limit.
    fn trim(&self, conn: &rusqlite::Connection, bytes: &mut i64) {
        let limit = (self.limit)();
        if limit <= 0 || *bytes <= limit {
            return;
        }
        let target = (limit as f64 * LOW_WATER) as i64;
        let result = (|| -> rusqlite::Result<()> {
            // Walk from the oldest until enough is freed, then delete up to that id.
            let mut stmt =
                conn.prepare(&format!("SELECT id, {SIZE} FROM activity ORDER BY id ASC"))?;
            let mut rows = stmt.query([])?;
            let (mut threshold, mut freed) = (0i64, 0i64);
            while let Some(row) = rows.next()? {
                threshold = row.get(0)?;
                freed += row.get::<_, i64>(1)?;
                if *bytes - freed <= target {
                    break;
                }
            }
            if freed > 0 {
                conn.execute("DELETE FROM activity WHERE id <= ?", [threshold])?;
                *bytes -= freed;
            }
            Ok(())
        })();
        if let Err(err) = result {
            tracing::warn!("activity: trimming: {err}");
        }
    }

    /// At most `limit` entries, newest first.
    pub fn entries(&self, limit: usize) -> Vec<Entry> {
        let conn = self.db.lock();
        let read = || -> rusqlite::Result<Vec<Entry>> {
            let mut stmt = conn.prepare(
                "SELECT id, at, kind, summary, payload, acked, error, printer_id FROM activity ORDER BY id DESC LIMIT ?",
            )?;
            stmt.query_map([limit as i64], |row| {
                Ok(Entry {
                    id: row.get(0)?,
                    at: row.get(1)?,
                    kind: row.get(2)?,
                    summary: row.get(3)?,
                    payload: row.get(4)?,
                    acked: row.get(5)?,
                    error: row.get(6)?,
                    printer: row.get(7)?,
                })
            })?
            .collect()
        };
        read().unwrap_or_else(|err| {
            tracing::warn!("activity: reading: {err}");
            Vec::new()
        })
    }

    #[cfg(test)]
    fn bytes(&self) -> i64 {
        *self.bytes.lock().unwrap()
    }
}

fn total(db: &Db) -> rusqlite::Result<i64> {
    db.lock().query_row(
        &format!("SELECT COALESCE(SUM({SIZE}), 0) FROM activity"),
        [],
        |r| r.get(0),
    )
}

/// Cuts a payload to MAX_PAYLOAD bytes, at a character boundary.
fn truncate(payload: &str) -> std::borrow::Cow<'_, str> {
    if payload.len() <= MAX_PAYLOAD {
        return payload.into();
    }
    let mut end = MAX_PAYLOAD;
    while !payload.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… (truncated)", &payload[..end]).into()
}

impl Source for Log {
    fn name(&self) -> &'static str {
        "event log"
    }

    fn oldest(&self, n: usize) -> rusqlite::Result<Vec<Item>> {
        let conn = self.db.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT id, at, {SIZE} FROM activity ORDER BY id ASC LIMIT ?"
        ))?;
        stmt.query_map([n as i64], |r| {
            Ok(Item {
                id: r.get(0)?,
                when: r.get(1)?,
                bytes: r.get(2)?,
            })
        })?
        .collect()
    }

    /// Removes every entry up to and including `id`, then recounts.
    fn delete_through(&self, id: i64) -> rusqlite::Result<()> {
        let mut bytes = self.bytes.lock().unwrap_or_else(|p| p.into_inner());
        self.db
            .lock()
            .execute("DELETE FROM activity WHERE id <= ?", [id])?;
        *bytes = total(&self.db)?;
        Ok(())
    }

    fn total_bytes(&self) -> rusqlite::Result<i64> {
        total(&self.db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock;

    fn log(limit: i64) -> (Log, Db, clock::Fake) {
        let db = Db::memory();
        let fake = clock::Fake::at(1_700_000_000_000);
        (
            Log::new(db.clone(), move || limit, fake.clock()).unwrap(),
            db,
            fake,
        )
    }

    #[test]
    fn record_and_read_back_newest_first() {
        let (log, _, fake) = log(1 << 20);
        let first = log.record(COMMAND, "home", "{}");
        fake.advance_ms(5);
        log.record(REPORT, "report", "{\"print\":{}}");
        log.acknowledge(first, Ok(1_700_000_000_002));
        let entries = log.entries(10);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, REPORT);
        assert_eq!(entries[0].at, 1_700_000_000_005);
        assert_eq!(entries[1].summary, "home");
        assert_eq!(entries[1].acked, Some(1_700_000_000_002));
        assert_eq!(log.entries(1).len(), 1);
    }

    #[test]
    fn errors_are_recorded() {
        let (log, _, _) = log(1 << 20);
        let id = log.record(COMMAND, "stop", "{}");
        log.acknowledge(id, Err("no acknowledgement".into()));
        let e = &log.entries(1)[0];
        assert_eq!(e.error, "no acknowledgement");
        assert_eq!(e.acked, None);
        assert_eq!(
            log.bytes(),
            ("stop".len() + 2 + "no acknowledgement".len()) as i64 + ROW_OVERHEAD
        );
    }

    #[test]
    fn serialises_like_the_page_expects() {
        let (log, _, _) = log(1 << 20);
        log.record(NOTIFICATION, "x", "");
        let json = serde_json::to_value(&log.entries(1)[0]).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.contains_key("at") && obj["at"].as_str().unwrap().starts_with("20"));
        assert!(
            !obj.contains_key("payload")
                && !obj.contains_key("acked")
                && !obj.contains_key("error")
        );
    }

    #[test]
    fn long_payloads_are_cut_at_a_character_boundary() {
        let (log, _, _) = log(1 << 20);
        let payload = format!("a{}", "é".repeat(3000));
        log.record(REPORT, "report", &payload);
        let stored = &log.entries(1)[0].payload;
        assert!(stored.ends_with("… (truncated)"));
        assert!(stored.len() <= MAX_PAYLOAD + "… (truncated)".len());
    }

    #[test]
    fn trims_oldest_to_under_the_limit() {
        let (log, db, _) = log(10_000);
        for i in 0..200 {
            log.record(REPORT, "report", &format!("{i:0>100}"));
        }
        assert!(log.bytes() <= 10_000);
        assert_eq!(log.bytes(), total(&db).unwrap());
        let entries = log.entries(1000);
        assert_eq!(entries[0].payload, format!("{:0>100}", 199));
        assert!(entries.len() < 200);
    }

    #[test]
    fn count_survives_reopening() {
        let (log, db, fake) = log(1 << 20);
        log.record(REPORT, "report", "abc");
        let again = Log::new(db, || 1 << 20, fake.clock()).unwrap();
        assert_eq!(again.bytes(), log.bytes());
    }

    #[test]
    fn source_for_the_size_cap() {
        let (log, _, _) = log(1 << 20);
        let a = log.record(REPORT, "a", "1").unwrap();
        log.record(REPORT, "b", "22");
        let items = log.oldest(10).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].bytes, 2 + ROW_OVERHEAD);
        log.delete_through(a).unwrap();
        assert_eq!(log.entries(10).len(), 1);
        assert_eq!(log.bytes(), 3 + ROW_OVERHEAD);
    }
}
