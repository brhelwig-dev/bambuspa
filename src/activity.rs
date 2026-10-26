use std::sync::{Arc, Mutex};

use bambuspa_api::Event;
use rusqlite::params;

use crate::clock::Clock;
use crate::db::Db;

pub const COMMAND: &str = "command";
pub const REPORT: &str = "report";
pub const NOTIFICATION: &str = "notification";

const MAX_PAYLOAD: usize = 4096;

const ROW_OVERHEAD: i64 = 64;

const LOW_WATER: f64 = 0.9;

const SIZE: &str = "octet_length(summary) + octet_length(payload) + octet_length(error) + 64";

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: i64,
    pub at: i64,
    pub kind: String,
    pub summary: String,
    pub payload: String,
    pub acked: Option<i64>,
    pub error: String,
    pub printer: Option<i64>,
}

impl From<&Entry> for Event {
    fn from(e: &Entry) -> Event {
        Event {
            id: e.id,
            at: to_rfc3339(e.at),
            kind: e.kind.clone(),
            summary: e.summary.clone(),
            payload: e.payload.clone(),
            acked: e.acked.map(to_rfc3339),
            error: e.error.clone(),
            printer: e.printer,
        }
    }
}

fn to_rfc3339(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, false))
        .unwrap_or_default()
}

#[derive(Clone)]
pub struct Log {
    db: Db,
    limit: Arc<dyn Fn() -> i64 + Send + Sync>,
    bytes: Arc<Mutex<i64>>,
    clock: Clock,
    printer: Option<i64>,
}

impl Log {
    pub fn new(
        db: Db,
        limit: impl Fn() -> i64 + Send + Sync + 'static,
        clock: Clock,
    ) -> rusqlite::Result<Log> {
        let bytes = total(&db)?;
        Ok(Log {
            db,
            limit: Arc::new(limit),
            bytes: Arc::new(Mutex::new(bytes)),
            clock,
            printer: None,
        })
    }

    pub fn for_printer(&self, id: i64) -> Log {
        Log {
            printer: Some(id),
            ..self.clone()
        }
    }

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

    pub fn acknowledge(&self, id: Option<i64>, result: Result<i64, String>) {
        let Some(id) = id else { return };
        let mut bytes = self.bytes.lock().unwrap_or_else(|p| p.into_inner());
        let conn = self.db.lock();
        match result {
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
                Ok(changed) if changed > 0 => {
                    *bytes += error.len() as i64;
                    self.trim(&conn, &mut bytes);
                }
                Ok(_) => {}
                Err(err) => tracing::warn!("activity: acknowledging {id}: {err}"),
            },
        }
    }

    fn trim(&self, conn: &rusqlite::Connection, bytes: &mut i64) {
        let limit = (self.limit)();
        if limit <= 0 || *bytes <= limit {
            return;
        }
        let target = (limit as f64 * LOW_WATER) as i64;
        let result = (|| -> rusqlite::Result<()> {
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
        let json = serde_json::to_value(Event::from(&log.entries(1)[0])).unwrap();
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
}
