//! The app's pending timers — heater and lamp shut-offs — plus the one clock
//! that is a start time rather than a deadline (when the bed came on). All are
//! stored in the database, since the deployment restarts to pick up a new
//! image and a restart must not cancel a countdown.
//!
//! The reactor sleeps until `earliest()` and re-plans whenever `changed()`
//! fires, so a deadline is acted on when it arrives rather than on a poll.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rusqlite::params;
use tokio::sync::Notify;

use crate::db::Db;

// Names of the timers. They are stored, so don't rename them.
pub const BED_OFF: &str = "bed-off";
pub const NOZZLE_OFF: &str = "nozzle-off";
pub const LAMP_OFF: &str = "lamp-off";
/// When the bed came on with no print running. A mark, not a deadline: it
/// never wakes anything.
pub const BED_ON_SINCE: &str = "bed-on-since";

const MARKS: &[&str] = &[BED_ON_SINCE];

/// Pending timers, in unix seconds. Write failures are logged and ignored: the
/// countdown still runs, it just won't survive a restart.
///
/// Each printer has its own set, from `for_printer`: the same names, stored as
/// "<printer id>:<name>".
#[derive(Clone)]
pub struct Timers {
    db: Db,
    at: Arc<Mutex<HashMap<String, i64>>>,
    prefix: Arc<str>,
    changed: Arc<Notify>,
}

impl Timers {
    /// Loads whatever was pending when the process last stopped.
    pub fn new(db: Db) -> rusqlite::Result<Timers> {
        let at = {
            let conn = db.lock();
            let mut stmt = conn.prepare("SELECT name, at FROM deadlines")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        Ok(Timers {
            db,
            at: Arc::new(Mutex::new(at)),
            prefix: "".into(),
            changed: Arc::default(),
        })
    }

    /// One printer's timers. Each call has its own `changed`, so a printer's
    /// reactor wakes for its own timers.
    pub fn for_printer(&self, id: i64) -> Timers {
        Timers {
            prefix: format!("{id}:").into(),
            changed: Arc::default(),
            ..self.clone()
        }
    }

    fn key(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    /// Forgets every timer in this set, for a printer that is removed.
    pub fn clear_all(&self) {
        let prefix = self.prefix.to_string();
        self.map().retain(|name, _| !name.starts_with(&prefix));
        if let Err(err) = self.db.lock().execute(
            "DELETE FROM deadlines WHERE substr(name, 1, length(?1)) = ?1",
            [&prefix],
        ) {
            tracing::warn!("timers: clearing {prefix}: {err}");
        }
        self.changed.notify_one();
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, i64>> {
        self.at.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Sets a timer, replacing any earlier setting for it.
    pub fn set(&self, name: &str, at: i64) {
        let name = &self.key(name);
        self.map().insert(name.clone(), at);
        if let Err(err) = self.db.lock().execute(
            "INSERT INTO deadlines (name, at) VALUES (?, ?) ON CONFLICT(name) DO UPDATE SET at = excluded.at",
            params![name, at],
        ) {
            tracing::warn!("timers: recording {name}: {err}");
        }
        self.changed.notify_one();
    }

    /// Forgets a timer. Clearing one that is not set does nothing.
    pub fn clear(&self, name: &str) {
        let name = &self.key(name);
        if self.map().remove(name).is_none() {
            return;
        }
        if let Err(err) = self
            .db
            .lock()
            .execute("DELETE FROM deadlines WHERE name = ?", [name])
        {
            tracing::warn!("timers: clearing {name}: {err}");
        }
        self.changed.notify_one();
    }

    pub fn get(&self, name: &str) -> Option<i64> {
        self.map().get(&self.key(name)).copied()
    }

    /// Whether `name` is set and has come due at `now`.
    pub fn due(&self, name: &str, now: i64) -> bool {
        self.get(name).is_some_and(|at| now >= at)
    }

    /// Whole seconds until `name` comes due (never negative), or None when it
    /// is not set. `now` is in milliseconds, rounded to the nearest second.
    pub fn remaining(&self, name: &str, now_ms: i64) -> Option<i64> {
        self.get(name)
            .map(|at| ((at * 1000 - now_ms + 500).div_euclid(1000)).max(0))
    }

    /// The soonest deadline, ignoring marks.
    pub fn earliest(&self) -> Option<i64> {
        self.map()
            .iter()
            .filter_map(|(name, at)| Some((name.strip_prefix(&*self.prefix)?, at)))
            .filter(|(name, _)| !name.contains(':') && !MARKS.contains(name))
            .map(|(_, at)| *at)
            .min()
    }

    /// Completes the next time a timer is set or cleared.
    pub async fn changed(&self) {
        self.changed.notified().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_clear_and_survive_restart() {
        let db = Db::memory();
        let t = Timers::new(db.clone()).unwrap();
        t.set(BED_OFF, 100);
        t.set(BED_OFF, 200);
        t.set(LAMP_OFF, 50);
        t.set(BED_ON_SINCE, 10);
        t.clear(LAMP_OFF);
        t.clear(NOZZLE_OFF);
        let again = Timers::new(db).unwrap();
        assert_eq!(again.get(BED_OFF), Some(200));
        assert_eq!(again.get(LAMP_OFF), None);
        assert_eq!(again.get(BED_ON_SINCE), Some(10));
    }

    #[test]
    fn earliest_ignores_marks() {
        let t = Timers::new(Db::memory()).unwrap();
        assert_eq!(t.earliest(), None);
        t.set(BED_ON_SINCE, 1);
        assert_eq!(t.earliest(), None);
        t.set(NOZZLE_OFF, 30);
        t.set(BED_OFF, 20);
        assert_eq!(t.earliest(), Some(20));
    }

    #[test]
    fn remaining_and_due() {
        let t = Timers::new(Db::memory()).unwrap();
        assert_eq!(t.remaining(BED_OFF, 0), None);
        t.set(BED_OFF, 100);
        assert_eq!(t.remaining(BED_OFF, 40_400), Some(60));
        assert_eq!(t.remaining(BED_OFF, 40_600), Some(59));
        assert_eq!(t.remaining(BED_OFF, 200_000), Some(0));
        assert!(!t.due(BED_OFF, 99));
        assert!(t.due(BED_OFF, 100));
    }

    #[test]
    fn each_printer_has_its_own() {
        let db = Db::memory();
        let all = Timers::new(db.clone()).unwrap();
        let (one, two) = (all.for_printer(1), all.for_printer(2));
        one.set(BED_OFF, 100);
        two.set(BED_OFF, 50);
        two.set(LAMP_OFF, 70);
        assert_eq!((one.get(BED_OFF), two.get(BED_OFF)), (Some(100), Some(50)));
        assert_eq!((one.earliest(), two.earliest()), (Some(100), Some(50)));
        two.clear_all();
        assert_eq!((one.get(BED_OFF), two.get(LAMP_OFF)), (Some(100), None));
        let again = Timers::new(db).unwrap();
        assert_eq!(again.for_printer(1).get(BED_OFF), Some(100));
        assert_eq!(again.for_printer(2).get(BED_OFF), None);
    }

    #[tokio::test]
    async fn changes_wake_a_waiter() {
        let t = Timers::new(Db::memory()).unwrap();
        let waiter = tokio::spawn({
            let t = t.clone();
            async move { t.changed().await }
        });
        tokio::task::yield_now().await;
        t.set(LAMP_OFF, 5);
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
    }
}
