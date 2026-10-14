//! Recorded camera frames and print-job boundaries, so the app can serve a
//! scrollback buffer and per-print timelapses.

use rusqlite::{OptionalExtension, params};

use crate::capacity::{Item, Source};
use crate::db::Db;
use crate::p1s;

/// One print's recorded time range, in unix seconds. `end` is None while it is
/// still in progress.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Job {
    pub id: i64,
    pub name: String,
    pub start: i64,
    pub end: Option<i64>,
}

/// Camera frames and job boundaries. `new` reads and writes as printer 0,
/// which only tests use; `for_printer` gives one printer's. Making room
/// (`Source`) and forgetting jobs work across every printer.
#[derive(Clone)]
pub struct Store {
    db: Db,
    printer: i64,
}

impl Store {
    pub fn new(db: Db) -> Store {
        Store { db, printer: 0 }
    }

    pub fn for_printer(&self, id: i64) -> Store {
        Store {
            db: self.db.clone(),
            printer: id,
        }
    }

    /// Deletes this printer's footage and jobs, for a printer that is removed.
    pub fn delete_all(&self) -> rusqlite::Result<()> {
        let conn = self.db.lock();
        conn.execute("DELETE FROM frames WHERE printer_id = ?", [self.printer])?;
        conn.execute("DELETE FROM jobs WHERE printer_id = ?", [self.printer])?;
        Ok(())
    }

    /// Records one camera frame at a unix-second timestamp.
    pub fn insert_frame(&self, ts: i64, jpeg: &[u8]) -> rusqlite::Result<()> {
        self.db.lock().execute(
            "INSERT INTO frames (printer_id, ts, jpeg) VALUES (?, ?, ?)",
            params![self.printer, ts, jpeg],
        )?;
        Ok(())
    }

    /// The frame with the smallest timestamp at or after `ts`, with that
    /// timestamp.
    pub fn frame_at_or_after(&self, ts: i64) -> rusqlite::Result<Option<(Vec<u8>, i64)>> {
        self.db
            .lock()
            .query_row(
                "SELECT jpeg, ts FROM frames WHERE printer_id = ? AND ts >= ? ORDER BY ts ASC LIMIT 1",
                [self.printer, ts],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
    }

    /// The oldest and newest frame timestamps stored.
    pub fn range(&self) -> rusqlite::Result<(Option<i64>, Option<i64>)> {
        self.db.lock().query_row(
            "SELECT MIN(ts), MAX(ts) FROM frames WHERE printer_id = ?",
            [self.printer],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    /// Forgets prints whose footage has all been deleted to make room, so the
    /// list only offers what can still be played.
    pub fn forget_unrecorded_jobs(&self) -> rusqlite::Result<usize> {
        self.db.lock().execute(
            "DELETE FROM jobs WHERE end_ts IS NOT NULL
               AND end_ts < COALESCE(
                 (SELECT MIN(ts) FROM frames WHERE frames.printer_id = jobs.printer_id),
                 end_ts + 1)",
            [],
        )
    }

    /// Records the start of a print and returns its id.
    pub fn open_job(&self, name: &str, start: i64) -> rusqlite::Result<i64> {
        let conn = self.db.lock();
        conn.execute(
            "INSERT INTO jobs (printer_id, name, start_ts, end_ts) VALUES (?, ?, ?, NULL)",
            params![self.printer, name, start],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn close_job(&self, id: i64, end: i64) -> rusqlite::Result<()> {
        self.db
            .lock()
            .execute("UPDATE jobs SET end_ts = ? WHERE id = ?", params![end, id])?;
        Ok(())
    }

    /// The print still in progress — the newest row with no end — if any.
    pub fn active_job(&self) -> rusqlite::Result<Option<Job>> {
        self.db
            .lock()
            .query_row(
                "SELECT id, name, start_ts FROM jobs WHERE printer_id = ? AND end_ts IS NULL
                   ORDER BY start_ts DESC, id DESC LIMIT 1",
                [self.printer],
                |r| {
                    Ok(Job {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        start: r.get(2)?,
                        end: None,
                    })
                },
            )
            .optional()
    }

    /// Closes job `id` at the last frame recorded inside it, or at `fallback`
    /// when it has none. For a row left open by a crash, that is when the print
    /// actually stopped being recorded.
    pub fn close_job_at_last_frame(&self, id: i64, fallback: i64) -> rusqlite::Result<()> {
        let end = {
            let conn = self.db.lock();
            let start: i64 =
                conn.query_row("SELECT start_ts FROM jobs WHERE id = ?", [id], |r| r.get(0))?;
            // Bounded at the next print's start so one row can't claim the
            // footage of everything recorded after it.
            let last: Option<i64> = conn.query_row(
                "SELECT MAX(ts) FROM frames WHERE printer_id = ?3 AND ts >= ?1
                   AND ts < COALESCE(
                     (SELECT MIN(start_ts) FROM jobs WHERE printer_id = ?3 AND start_ts > ?1), ?2)",
                params![start, i64::MAX, self.printer],
                |r| r.get(0),
            )?;
            last.filter(|&last| last >= start).unwrap_or(fallback)
        };
        self.close_job(id, end)
    }

    /// Closes every open job row except the newest, reporting how many it
    /// closed. Only one print runs at a time, so any others are stale.
    pub fn close_orphan_jobs(&self) -> rusqlite::Result<usize> {
        let open: Vec<(i64, i64)> = {
            let conn = self.db.lock();
            let mut stmt =
                conn.prepare("SELECT id, start_ts FROM jobs WHERE printer_id = ? AND end_ts IS NULL ORDER BY start_ts DESC, id DESC")?;
            stmt.query_map([self.printer], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        // [0] is the newest: leave it open.
        for &(id, start) in open.iter().skip(1) {
            self.close_job_at_last_frame(id, start)?;
        }
        Ok(open.len().saturating_sub(1))
    }

    /// Every stored job, newest-started first (ties by insertion order).
    /// `forget_unrecorded_jobs` keeps this bounded.
    pub fn recent_jobs(&self) -> rusqlite::Result<Vec<Job>> {
        let conn = self.db.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, start_ts, end_ts FROM jobs WHERE printer_id = ? ORDER BY start_ts DESC, id DESC",
        )?;
        stmt.query_map([self.printer], |r| {
            Ok(Job {
                id: r.get(0)?,
                name: r.get(1)?,
                start: r.get(2)?,
                end: r.get(3)?,
            })
        })?
        .collect()
    }
}

impl Source for Store {
    fn name(&self) -> &'static str {
        "camera frames"
    }

    /// Ordered by id to match `delete_through`. `when` is in milliseconds,
    /// like the event log's.
    fn oldest(&self, n: usize) -> rusqlite::Result<Vec<Item>> {
        let conn = self.db.lock();
        let mut stmt =
            conn.prepare("SELECT id, ts, octet_length(jpeg) FROM frames ORDER BY id ASC LIMIT ?")?;
        stmt.query_map([n as i64], |r| {
            Ok(Item {
                id: r.get(0)?,
                when: r.get::<_, i64>(1)? * 1000,
                bytes: r.get(2)?,
            })
        })?
        .collect()
    }

    /// Removes every frame up to and including `id`, a running print's too.
    fn delete_through(&self, id: i64) -> rusqlite::Result<()> {
        self.db
            .lock()
            .execute("DELETE FROM frames WHERE id <= ?", [id])?;
        Ok(())
    }

    fn total_bytes(&self) -> rusqlite::Result<i64> {
        self.db.lock().query_row(
            "SELECT COALESCE(SUM(octet_length(jpeg)), 0) FROM frames",
            [],
            |r| r.get(0),
        )
    }
}

/// Opens and closes job rows as the printer's state moves, so frames can be
/// grouped and played back per print.
pub struct JobTracker {
    store: Store,
    open: Option<(i64, String)>,
}

impl JobTracker {
    /// Adopts a row left open by an earlier process, so a restart mid-print
    /// doesn't record the print twice, and closes any older open rows.
    pub fn new(store: Store) -> JobTracker {
        match store.close_orphan_jobs() {
            Ok(0) => {}
            Ok(n) => {
                tracing::info!("history: closed {n} job row(s) left open by an earlier process")
            }
            Err(err) => tracing::warn!("history: closing orphan jobs: {err}"),
        }
        let open = match store.active_job() {
            Ok(job) => job.map(|j| (j.id, j.name)),
            Err(err) => {
                tracing::warn!("history: adopting the open job: {err}");
                None
            }
        };
        JobTracker { store, open }
    }

    /// Opens a row when a print starts and closes it when it ends, returning
    /// whether anything changed. Repeated calls in one state change nothing.
    /// States that are neither running nor ended ("unknown", PREPARE) leave the
    /// row alone.
    pub fn observe(&mut self, gcode_state: &str, job_name: &str, now: i64) -> bool {
        let mut changed = false;
        if p1s::job_active(gcode_state) {
            // A different name means a job boundary was missed (e.g. while the
            // app was down), so start a new row. An empty name is just a
            // partial report.
            if self
                .open
                .as_ref()
                .is_some_and(|(_, open)| !job_name.is_empty() && job_name != open)
            {
                changed |= self.close(now);
            }
            if self.open.is_none() {
                match self.store.open_job(job_name, now) {
                    Ok(id) => {
                        self.open = Some((id, job_name.to_string()));
                        changed = true;
                    }
                    Err(err) => tracing::warn!("history: opening a job: {err}"),
                }
            }
        } else if p1s::job_ended(gcode_state) && self.open.is_some() {
            changed |= self.close(now);
        }
        changed
    }

    fn close(&mut self, now: i64) -> bool {
        let Some((id, _)) = self.open else {
            return false;
        };
        match self.store.close_job_at_last_frame(id, now) {
            Ok(()) => {
                self.open = None;
                true
            }
            // Left open on purpose: the next report in the same state retries.
            Err(err) => {
                tracing::warn!("history: closing a job: {err}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::new(Db::memory())
    }

    #[test]
    fn insert_and_frame_at_or_after() {
        let s = store();
        s.insert_frame(100, &[1]).unwrap();
        s.insert_frame(200, &[2]).unwrap();
        assert_eq!(s.frame_at_or_after(150).unwrap(), Some((vec![2], 200)));
        assert_eq!(s.frame_at_or_after(100).unwrap(), Some((vec![1], 100)));
        assert_eq!(s.frame_at_or_after(201).unwrap(), None);
    }

    #[test]
    fn range() {
        let s = store();
        assert_eq!(s.range().unwrap(), (None, None));
        s.insert_frame(300, &[1]).unwrap();
        s.insert_frame(100, &[1]).unwrap();
        assert_eq!(s.range().unwrap(), (Some(100), Some(300)));
    }

    #[test]
    fn jobs_go_once_their_footage_has() {
        let s = store();
        let gone = s.open_job("gone.3mf", 100).unwrap();
        s.close_job(gone, 200).unwrap();
        let kept = s.open_job("kept.3mf", 300).unwrap();
        s.close_job(kept, 400).unwrap();
        s.open_job("running.3mf", 500).unwrap();
        s.insert_frame(350, &[1]).unwrap();
        assert_eq!(s.forget_unrecorded_jobs().unwrap(), 1);
        let names: Vec<_> = s
            .recent_jobs()
            .unwrap()
            .into_iter()
            .map(|j| j.name)
            .collect();
        assert_eq!(names, ["running.3mf", "kept.3mf"]);
        // With no footage at all, only the running print is left.
        s.delete_through(i64::MAX).unwrap();
        s.forget_unrecorded_jobs().unwrap();
        assert_eq!(s.recent_jobs().unwrap().len(), 1);
        assert_eq!(s.total_bytes().unwrap(), 0);
    }

    #[test]
    fn each_printer_sees_its_own() {
        let all = store();
        let (one, two) = (all.for_printer(1), all.for_printer(2));
        one.insert_frame(100, &[1]).unwrap();
        two.insert_frame(200, &[2]).unwrap();
        one.open_job("one.3mf", 90).unwrap();
        two.open_job("two.3mf", 190).unwrap();
        assert_eq!(one.range().unwrap(), (Some(100), Some(100)));
        assert_eq!(two.frame_at_or_after(0).unwrap(), Some((vec![2], 200)));
        assert_eq!(one.active_job().unwrap().unwrap().name, "one.3mf");
        // Only one print runs per printer, not across them.
        assert_eq!(two.close_orphan_jobs().unwrap(), 0);
        assert_eq!(all.total_bytes().unwrap(), 2);
        two.delete_all().unwrap();
        assert_eq!(two.range().unwrap(), (None, None));
        assert!(two.recent_jobs().unwrap().is_empty());
        assert_eq!(one.recent_jobs().unwrap().len(), 1);
    }

    #[test]
    fn job_lifecycle() {
        let s = store();
        let id = s.open_job("benchy.3mf", 100).unwrap();
        assert_eq!(
            s.recent_jobs().unwrap(),
            vec![Job {
                id,
                name: "benchy.3mf".into(),
                start: 100,
                end: None
            }]
        );
        s.close_job(id, 200).unwrap();
        assert_eq!(s.recent_jobs().unwrap()[0].end, Some(200));
    }

    #[test]
    fn active_job() {
        let s = store();
        assert_eq!(s.active_job().unwrap(), None);
        let done = s.open_job("done.3mf", 100).unwrap();
        s.close_job(done, 200).unwrap();
        assert_eq!(s.active_job().unwrap(), None);
        let open = s.open_job("running.3mf", 300).unwrap();
        assert_eq!(
            s.active_job().unwrap(),
            Some(Job {
                id: open,
                name: "running.3mf".into(),
                start: 300,
                end: None
            })
        );
    }

    #[test]
    fn close_orphan_jobs_leaves_a_single_open_row_alone() {
        let s = store();
        s.open_job("running.3mf", 1000).unwrap();
        assert_eq!(s.close_orphan_jobs().unwrap(), 0);
        assert_eq!(s.recent_jobs().unwrap()[0].end, None);
    }

    #[test]
    fn close_orphan_jobs_closes_stranded_rows_at_their_last_frame() {
        let s = store();
        s.open_job("pencil.3mf", 1000).unwrap();
        s.open_job("pencil.3mf", 2000).unwrap();
        s.open_job("running.3mf", 3000).unwrap();
        for ts in [1000, 1500, 1900, 2000, 2400, 3000, 3500] {
            s.insert_frame(ts, &[1]).unwrap();
        }
        assert_eq!(s.close_orphan_jobs().unwrap(), 2);
        let jobs = s.recent_jobs().unwrap();
        assert_eq!((jobs[0].name.as_str(), jobs[0].end), ("running.3mf", None));
        assert_eq!(jobs[1].end, Some(2400));
        assert_eq!(jobs[2].end, Some(1900));
    }

    #[test]
    fn close_orphan_jobs_falls_back_to_start_without_footage() {
        let s = store();
        s.open_job("footage-already-pruned.3mf", 1000).unwrap();
        s.open_job("running.3mf", 5000).unwrap();
        s.close_orphan_jobs().unwrap();
        let jobs = s.recent_jobs().unwrap();
        assert_eq!(jobs[1].end, Some(1000));
    }

    // The tracker, which replaces the old polling job watcher.

    fn tracked() -> (Store, JobTracker) {
        let s = store();
        let t = JobTracker::new(s.clone());
        (s, t)
    }

    #[test]
    fn tracker_opens_and_closes_on_transition() {
        let (s, mut t) = tracked();
        assert!(!t.observe("IDLE", "", 100));
        assert!(t.observe("RUNNING", "benchy.3mf", 200));
        assert!(!t.observe("RUNNING", "benchy.3mf", 300));
        assert!(t.observe("FINISH", "benchy.3mf", 400));
        assert!(!t.observe("FINISH", "benchy.3mf", 500));
        let jobs = s.recent_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!((jobs[0].start, jobs[0].end), (200, Some(400)));
    }

    #[test]
    fn tracker_keeps_one_row_across_pause_and_resume() {
        let (s, mut t) = tracked();
        t.observe("RUNNING", "a", 100);
        t.observe("PAUSE", "a", 200);
        t.observe("RUNNING", "a", 300);
        assert_eq!(s.recent_jobs().unwrap().len(), 1);
        assert_eq!(s.recent_jobs().unwrap()[0].end, None);
    }

    #[test]
    fn tracker_adopts_a_row_left_open_by_an_earlier_process() {
        let s = store();
        s.open_job("a", 100).unwrap();
        let mut t = JobTracker::new(s.clone());
        t.observe("RUNNING", "a", 200);
        assert_eq!(s.recent_jobs().unwrap().len(), 1);
        t.observe("FINISH", "a", 300);
        assert_eq!(s.recent_jobs().unwrap()[0].end, Some(300));
    }

    #[test]
    fn tracker_closes_rows_stranded_by_an_earlier_process() {
        let s = store();
        s.open_job("a", 100).unwrap();
        s.open_job("b", 200).unwrap();
        JobTracker::new(s.clone());
        let jobs = s.recent_jobs().unwrap();
        assert_eq!((jobs[0].end, jobs[1].end), (None, Some(100)));
    }

    #[test]
    fn tracker_starts_a_fresh_row_when_a_different_print_is_running() {
        let (s, mut t) = tracked();
        t.observe("RUNNING", "a", 100);
        t.observe("RUNNING", "b", 200);
        let jobs = s.recent_jobs().unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!((jobs[0].name.as_str(), jobs[0].end), ("b", None));
        assert_eq!(jobs[1].end, Some(200));
    }

    #[test]
    fn tracker_keeps_the_open_row_when_the_name_is_missing() {
        let (s, mut t) = tracked();
        t.observe("RUNNING", "a", 100);
        t.observe("RUNNING", "", 200);
        assert_eq!(s.recent_jobs().unwrap().len(), 1);
    }

    #[test]
    fn tracker_ignores_unknown_and_prepare() {
        let (s, mut t) = tracked();
        t.observe("unknown", "", 100);
        assert!(s.recent_jobs().unwrap().is_empty());
        t.observe("RUNNING", "a", 200);
        t.observe("PREPARE", "a", 300);
        t.observe("unknown", "a", 400);
        assert_eq!(s.recent_jobs().unwrap()[0].end, None);
    }

    #[test]
    fn tracker_closes_at_the_last_recorded_frame_or_now() {
        let (s, mut t) = tracked();
        t.observe("RUNNING", "a", 100);
        s.insert_frame(150, &[1]).unwrap();
        t.observe("FINISH", "a", 500);
        t.observe("RUNNING", "b", 600);
        t.observe("FAILED", "b", 700);
        let jobs = s.recent_jobs().unwrap();
        assert_eq!(jobs[1].end, Some(150));
        assert_eq!(jobs[0].end, Some(700));
    }
}
