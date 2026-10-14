//! The printers the app looks after, each with a name the user chose. Every
//! printer has its own connection, camera recorder, reactor (notifications,
//! auto-off, the lamp) and live feed, all running whichever printer the page
//! is showing.

use std::sync::{Arc, RwLock};

use rusqlite::params;
use serde_json::{Value, json};
use tokio::sync::{Notify, watch};
use tokio::task::AbortHandle;

use crate::activity::Log;
use crate::camera::Hub;
use crate::clock::Clock;
use crate::db::Db;
use crate::history::{JobTracker, Store};
use crate::p1s::{self, Config, Link, Ports, StateCache};
use crate::push::Sender;
use crate::settings::Settings;
use crate::timers::Timers;
use crate::web::Feed;

/// The longest name a printer may have, in characters.
pub const MAX_NAME: usize = 40;

/// One printer while the app runs.
pub struct Printer {
    pub id: i64,
    name: RwLock<String>,
    pub link: Link,
    pub cache: StateCache,
    pub store: Store,
    pub timers: Timers,
    pub hub: Hub,
    pub feed: Feed,
    /// Fires when one of its job rows is opened, closed or forgotten.
    pub jobs_changed: Arc<Notify>,
}

impl Printer {
    pub fn name(&self) -> String {
        self.name.read().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// What every printer is built from.
#[derive(Clone)]
pub struct Deps {
    pub db: Db,
    pub settings: Settings,
    pub log: Log,
    pub sender: Sender,
    pub store: Store,
    pub timers: Timers,
    pub clock: Clock,
    pub ports: Ports,
}

/// A running printer and the tasks that belong to it.
struct Entry {
    printer: Arc<Printer>,
    tasks: Vec<AbortHandle>,
}

impl Drop for Entry {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        self.printer.link.stop();
    }
}

struct Inner {
    deps: Deps,
    /// In the order they were added.
    entries: RwLock<Vec<Entry>>,
    /// `{"type":"printers","printers":[...]}`: each printer's name and how it
    /// is doing, for the switcher. Republished whenever any of it changes.
    summary: watch::Sender<Arc<str>>,
}

#[derive(Clone)]
pub struct Printers {
    inner: Arc<Inner>,
}

impl Printers {
    /// Starts every stored printer.
    pub fn load(deps: Deps) -> rusqlite::Result<Printers> {
        let rows: Vec<(i64, String, Config)> = {
            let conn = deps.db.lock();
            let mut stmt =
                conn.prepare("SELECT id, name, ip, serial, access_code FROM printers ORDER BY id")?;
            stmt.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    Config {
                        ip: r.get(2)?,
                        serial: r.get(3)?,
                        access_code: r.get(4)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        let printers = Printers {
            inner: Arc::new(Inner {
                deps,
                entries: RwLock::default(),
                summary: watch::Sender::new(Arc::from("")),
            }),
        };
        for (id, name, conf) in rows {
            printers.start(id, name, conf);
        }
        printers.refresh_summary();
        Ok(printers)
    }

    fn start(&self, id: i64, name: String, conf: Config) {
        let d = &self.inner.deps;
        let cache = StateCache::new();
        let link = Link::new(
            cache.clone(),
            d.log.for_printer(id),
            d.clock.clone(),
            d.ports,
        );
        link.configure(conf);
        let store = d.store.for_printer(id);
        let timers = d.timers.for_printer(id);
        let hub = Hub::new(link.clone(), store.clone(), d.clock.clone());
        let printer = Arc::new(Printer {
            id,
            name: RwLock::new(name),
            link: link.clone(),
            cache: cache.clone(),
            store: store.clone(),
            timers: timers.clone(),
            hub: hub.clone(),
            feed: Feed::default(),
            jobs_changed: Arc::default(),
        });
        let settings = d.settings.clone();
        let reactor =
            crate::core::Reactor::new(timers, JobTracker::new(store), move || settings.values());
        let runner = crate::core::Runner {
            link,
            sender: d.sender.clone(),
            jobs_changed: printer.jobs_changed.clone(),
            clock: d.clock.clone(),
            printer: id,
            printers: self.clone(),
        };
        let tasks = vec![
            tokio::spawn(reactor.run(cache.subscribe(), runner)).abort_handle(),
            tokio::spawn(hub.run()).abort_handle(),
            tokio::spawn(
                printer
                    .feed
                    .clone()
                    .run(printer.clone(), self.clone(), d.clock.clone()),
            )
            .abort_handle(),
        ];
        self.write().push(Entry { printer, tasks });
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<Entry>> {
        self.inner.entries.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Vec<Entry>> {
        self.inner
            .entries
            .write()
            .unwrap_or_else(|p| p.into_inner())
    }

    pub fn list(&self) -> Vec<Arc<Printer>> {
        self.read().iter().map(|e| e.printer.clone()).collect()
    }

    pub fn get(&self, id: i64) -> Option<Arc<Printer>> {
        self.read()
            .iter()
            .find(|e| e.printer.id == id)
            .map(|e| e.printer.clone())
    }

    pub fn first(&self) -> Option<Arc<Printer>> {
        self.read().first().map(|e| e.printer.clone())
    }

    /// What starts a notification about printer `id`: its name, once there is
    /// more than one printer to tell apart.
    pub fn label(&self, id: i64) -> String {
        let entries = self.read();
        if entries.len() < 2 {
            return String::new();
        }
        entries
            .iter()
            .find(|e| e.printer.id == id)
            .map(|e| format!("{}: ", e.printer.name()))
            .unwrap_or_default()
    }

    /// Checks a name for a printer other than `except`, and returns it tidied.
    pub fn check_name(&self, name: &str, except: Option<i64>) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("give the printer a name".into());
        }
        if name.chars().count() > MAX_NAME {
            return Err(format!(
                "a printer's name can be at most {MAX_NAME} characters"
            ));
        }
        let taken = self.read().iter().any(|e| {
            Some(e.printer.id) != except && e.printer.name().to_lowercase() == name.to_lowercase()
        });
        if taken {
            return Err(format!("there is already a printer called {name}"));
        }
        Ok(name.to_string())
    }

    /// Stores and starts a new printer, returning its id.
    pub fn add(&self, name: &str, conf: Config) -> Result<i64, String> {
        let name = self.check_name(name, None)?;
        let id = {
            let conn = self.inner.deps.db.lock();
            conn.execute(
                "INSERT INTO printers (name, ip, serial, access_code) VALUES (?, ?, ?, ?)",
                params![name, conf.ip, conf.serial, conf.access_code],
            )
            .map_err(|e| e.to_string())?;
            conn.last_insert_rowid()
        };
        self.start(id, name, conf);
        self.refresh_summary();
        Ok(id)
    }

    /// Renames a printer and, if they changed, points it at new details. A
    /// rename alone leaves the connection alone.
    pub fn update(&self, id: i64, name: &str, conf: Config) -> Result<(), String> {
        let printer = self.get(id).ok_or("no such printer")?;
        let name = self.check_name(name, Some(id))?;
        self.inner
            .deps
            .db
            .lock()
            .execute(
                "UPDATE printers SET name = ?, ip = ?, serial = ?, access_code = ? WHERE id = ?",
                params![name, conf.ip, conf.serial, conf.access_code, id],
            )
            .map_err(|e| e.to_string())?;
        *printer.name.write().unwrap_or_else(|p| p.into_inner()) = name;
        if printer.link.config() != conf {
            printer.link.configure(conf);
        }
        self.refresh_summary();
        Ok(())
    }

    /// Stops a printer and deletes everything kept about it: its details,
    /// footage, jobs, timers and bed reminders. Its events stay in the log.
    pub fn remove(&self, id: i64) -> Result<(), String> {
        let entry = {
            let mut entries = self.write();
            let i = entries
                .iter()
                .position(|e| e.printer.id == id)
                .ok_or("no such printer")?;
            entries.remove(i)
        };
        let d = &self.inner.deps;
        d.db.lock()
            .execute("DELETE FROM printers WHERE id = ?", [id])
            .map_err(|e| e.to_string())?;
        entry.printer.timers.clear_all();
        entry
            .printer
            .store
            .delete_all()
            .map_err(|e| e.to_string())?;
        d.sender.forget_bed_reminders(id)?;
        drop(entry);
        self.refresh_summary();
        Ok(())
    }

    /// Whether a printer with this id is stored, read from the database.
    #[cfg(test)]
    pub fn stored(&self, id: i64) -> bool {
        self.inner
            .deps
            .db
            .lock()
            .query_row("SELECT 1 FROM printers WHERE id = ?", [id], |_| Ok(()))
            .is_ok()
    }

    /// Recomputes the switcher's summary, telling watchers only if it changed.
    pub fn refresh_summary(&self) {
        let printers: Vec<Value> = self
            .list()
            .iter()
            .map(|p| {
                let snap = p.cache.snapshot();
                let state = p1s::gcode_state(&snap.fields);
                json!({
                    "id": p.id,
                    "name": p.name(),
                    "connected": snap.connected,
                    "problem": snap.problem.as_deref(),
                    "gcodeState": state,
                    "progress": p1s::job_active(state)
                        .then(|| snap.fields.get("mc_percent").cloned())
                        .flatten(),
                })
            })
            .collect();
        let msg: Arc<str> = json!({"type": "printers", "printers": printers})
            .to_string()
            .into();
        self.inner.summary.send_if_modified(|current| {
            let changed = *current != msg;
            if changed {
                *current = msg;
            }
            changed
        });
    }

    pub fn summary(&self) -> watch::Receiver<Arc<str>> {
        self.inner.summary.subscribe()
    }

    /// Wakes every printer's job list, after housekeeping may have forgotten
    /// some.
    pub fn jobs_changed(&self) {
        for p in self.list() {
            p.jobs_changed.notify_one();
        }
    }

    /// The printers' MQTT port: fixed on a real printer, a fake's in tests.
    pub fn mqtt_port(&self) -> u16 {
        self.inner.deps.ports.mqtt
    }

    /// For the startup log line.
    pub fn describe(&self) -> String {
        match self.list().as_slice() {
            [] => "no printer set up".into(),
            [one] => format!("printer {} at {}", one.name(), one.link.config().ip),
            many => format!("{} printers", many.len()),
        }
    }

    /// Closes every connection, at shutdown.
    pub fn stop(&self) {
        for p in self.list() {
            p.link.stop();
        }
    }
}
