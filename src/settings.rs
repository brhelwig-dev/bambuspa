//! The app's configuration, stored in the database so it can be changed from
//! the page. Everything that consults a setting reads it at the point of use,
//! so an edit takes effect without a restart.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use rusqlite::params;

use crate::db::Db;

// Names of the settings, as the page sends them.
pub const CAMERA_STORAGE: &str = "camera-storage";
pub const BED_OFF_AFTER: &str = "bed-off-after";
pub const NOZZLE_OFF_AFTER: &str = "nozzle-off-after";
pub const LAMP_OFF_AFTER: &str = "lamp-off-after";
pub const ACTIVITY_LIMIT: &str = "activity-limit";
pub const SESSION_LENGTH: &str = "session-length";

/// The printer screen's sections, in order. Anything absent is hidden.
pub const DASHBOARD: &str = "dashboard";

const TEXTS: &[&str] = &[DASHBOARD];

/// Whether a setting holds words rather than a number.
pub fn is_text(name: &str) -> bool {
    TEXTS.contains(&name)
}

/// Converts the stored megabytes to bytes.
pub const BYTES_PER_MB: i64 = 1 << 20;

/// One complete set of settings. Durations are in seconds, limits in bytes.
/// The printers are kept in their own table.
#[derive(Clone, Debug, PartialEq)]
pub struct Values {
    pub dashboard: String,

    /// Bytes of camera frames kept, across every print; the oldest go first.
    pub camera_storage: i64,
    pub bed_off_after: i64,
    pub nozzle_off_after: i64,
    pub lamp_off_after: i64,
    pub activity_limit: i64,
    pub session_length: i64,
}

impl Default for Values {
    /// What an unconfigured app runs with, and what a value that cannot be
    /// read falls back to.
    fn default() -> Self {
        Values {
            dashboard: String::new(),
            camera_storage: 1024 * BYTES_PER_MB,
            bed_off_after: 24 * 3600,
            nozzle_off_after: 15 * 60,
            lamp_off_after: 8 * 3600,
            activity_limit: 64 * BYTES_PER_MB,
            session_length: 14 * 24 * 3600,
        }
    }
}

#[derive(Clone, Copy)]
enum Unit {
    Seconds,
    Megabytes,
}

/// A numeric setting's unit and allowed range.
#[derive(Clone, Copy)]
struct Spec {
    unit: Unit,
    min: i64,
    max: i64,
}

impl Spec {
    const fn new(unit: Unit, min: i64, max: i64) -> Spec {
        Spec { unit, min, max }
    }

    fn allows(&self, value: i64) -> bool {
        (self.min..=self.max).contains(&value)
    }

    /// Says what the setting will take, in the units it is written in.
    fn refuse(&self, name: &str) -> String {
        let (min, max) = (self.show(self.min), self.show(self.max));
        format!("{name} must be between {min} and {max}")
    }

    fn show(&self, value: i64) -> String {
        match self.unit {
            Unit::Seconds => show_duration(value),
            Unit::Megabytes => format!("{value} MB"),
        }
    }
}

/// Renders seconds as hours, minutes and seconds, e.g. `1h0m0s`.
fn show_duration(secs: i64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

const DAY: i64 = 24 * 3600;

fn spec(name: &str) -> Option<Spec> {
    Some(match name {
        CAMERA_STORAGE => Spec::new(Unit::Megabytes, 64, 64 * 1024),
        // The bed and lamp are set in whole hours on the Settings screen.
        BED_OFF_AFTER => Spec::new(Unit::Seconds, 3600, 7 * DAY),
        NOZZLE_OFF_AFTER => Spec::new(Unit::Seconds, 60, 7 * DAY),
        LAMP_OFF_AFTER => Spec::new(Unit::Seconds, 3600, 7 * DAY),
        ACTIVITY_LIMIT => Spec::new(Unit::Megabytes, 1, 512),
        SESSION_LENGTH => Spec::new(Unit::Seconds, DAY, 365 * DAY),
        _ => return None,
    })
}

/// Reads and writes the settings, caching the current values in memory.
#[derive(Clone)]
pub struct Settings {
    db: Db,
    values: Arc<RwLock<Values>>,
}

impl Settings {
    pub fn new(db: Db) -> rusqlite::Result<Settings> {
        let s = Settings {
            db,
            values: Arc::default(),
        };
        s.reload()?;
        Ok(s)
    }

    /// The current settings.
    pub fn values(&self) -> Values {
        self.values
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Stores one numeric setting, in seconds, megabytes or a count depending
    /// on the setting.
    pub fn set(&self, name: &str, value: i64) -> Result<(), String> {
        let spec = spec(name).ok_or_else(|| format!("settings: unknown setting {name:?}"))?;
        if !spec.allows(value) {
            return Err(spec.refuse(name));
        }
        self.put(name, &value.to_string())
    }

    /// Stores one setting that holds words. An empty value clears it.
    pub fn set_text(&self, name: &str, value: &str) -> Result<(), String> {
        if !is_text(name) {
            return Err(format!("settings: {name:?} does not hold text"));
        }
        if value.len() > 512 {
            return Err(format!("{name} is too long"));
        }
        if value.is_empty() {
            self.db
                .lock()
                .execute("DELETE FROM settings WHERE name = ?", [name])
                .map_err(|e| e.to_string())?;
            return self.reload().map_err(|e| e.to_string());
        }
        self.put(name, value)
    }

    fn put(&self, name: &str, value: &str) -> Result<(), String> {
        self.db
            .lock()
            .execute(
                "INSERT INTO settings (name, value) VALUES (?, ?)
                 ON CONFLICT(name) DO UPDATE SET value = excluded.value",
                params![name, value],
            )
            .map_err(|e| e.to_string())?;
        self.reload().map_err(|e| e.to_string())
    }

    /// Reads every setting back into memory. An invalid stored value falls
    /// back to its default.
    fn reload(&self) -> rusqlite::Result<()> {
        let stored: HashMap<String, String> = {
            let conn = self.db.lock();
            let mut stmt = conn.prepare("SELECT name, value FROM settings")?;
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let d = Values::default();
        let num = |name: &str, fallback: i64| {
            stored
                .get(name)
                .and_then(|raw| raw.parse::<i64>().ok())
                .filter(|n| spec(name).is_some_and(|s| s.allows(*n)))
                .unwrap_or(fallback)
        };
        let text = |name: &str| stored.get(name).cloned().unwrap_or_default();
        let v = Values {
            dashboard: text(DASHBOARD),
            camera_storage: num(CAMERA_STORAGE, d.camera_storage / BYTES_PER_MB) * BYTES_PER_MB,
            bed_off_after: num(BED_OFF_AFTER, d.bed_off_after),
            nozzle_off_after: num(NOZZLE_OFF_AFTER, d.nozzle_off_after),
            lamp_off_after: num(LAMP_OFF_AFTER, d.lamp_off_after),
            activity_limit: num(ACTIVITY_LIMIT, d.activity_limit / BYTES_PER_MB) * BYTES_PER_MB,
            session_length: num(SESSION_LENGTH, d.session_length),
        };
        *self.values.write().unwrap_or_else(|p| p.into_inner()) = v;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Settings {
        Settings::new(Db::memory()).unwrap()
    }

    #[test]
    fn defaults_when_nothing_stored() {
        assert_eq!(store().values(), Values::default());
    }

    #[test]
    fn set_is_read_back_in_its_units() {
        let s = store();
        s.set(CAMERA_STORAGE, 2048).unwrap();
        s.set(ACTIVITY_LIMIT, 2).unwrap();
        s.set(BED_OFF_AFTER, 7200).unwrap();
        let v = s.values();
        assert_eq!(v.camera_storage, 2048 * BYTES_PER_MB);
        assert_eq!(v.activity_limit, 2 * BYTES_PER_MB);
        assert_eq!(v.bed_off_after, 7200);
    }

    #[test]
    fn values_survive_reopening() {
        let db = Db::memory();
        Settings::new(db.clone())
            .unwrap()
            .set(NOZZLE_OFF_AFTER, 60)
            .unwrap();
        assert_eq!(Settings::new(db).unwrap().values().nozzle_off_after, 60);
    }

    #[test]
    fn refusals_read_in_the_settings_units() {
        let s = store();
        assert_eq!(
            s.set(BED_OFF_AFTER, 10).unwrap_err(),
            "bed-off-after must be between 1h0m0s and 168h0m0s"
        );
        assert_eq!(
            s.set(NOZZLE_OFF_AFTER, 1).unwrap_err(),
            "nozzle-off-after must be between 1m0s and 168h0m0s"
        );
        assert_eq!(
            s.set(CAMERA_STORAGE, 10).unwrap_err(),
            "camera-storage must be between 64 MB and 65536 MB"
        );
        assert_eq!(
            s.set(ACTIVITY_LIMIT, 0).unwrap_err(),
            "activity-limit must be between 1 MB and 512 MB"
        );
        assert_eq!(
            s.set("nope", 1).unwrap_err(),
            "settings: unknown setting \"nope\""
        );
        assert_eq!(s.values(), Values::default());
    }

    #[test]
    fn invalid_stored_values_fall_back() {
        let db = Db::memory();
        db.lock()
            .execute_batch(
                "INSERT INTO settings VALUES ('camera-storage', 'x'), ('lamp-off-after', '1')",
            )
            .unwrap();
        let v = Settings::new(db).unwrap().values();
        assert_eq!(v.camera_storage, Values::default().camera_storage);
        assert_eq!(v.lamp_off_after, Values::default().lamp_off_after);
    }

    #[test]
    fn text_settings() {
        let s = store();
        s.set_text(DASHBOARD, "camCard,jobCard").unwrap();
        assert_eq!(s.values().dashboard, "camCard,jobCard");
        s.set_text(DASHBOARD, "").unwrap();
        assert_eq!(s.values().dashboard, "");
        assert_eq!(
            s.set_text(DASHBOARD, &"x".repeat(513)).unwrap_err(),
            "dashboard is too long"
        );
        assert!(s.set_text(CAMERA_STORAGE, "1").is_err());
        assert!(is_text(DASHBOARD) && !is_text(CAMERA_STORAGE));
    }
}
