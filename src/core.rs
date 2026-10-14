//! What the app does on its own as the printer's state changes: records print
//! boundaries, sends notifications, drives the chamber lamp, shuts heaters off
//! after a while, and reminds about a bed left on.
//!
//! Every rule runs in one place, `Reactor::evaluate`, on every change to the
//! printer's state and whenever a timer comes due — so a reaction follows the
//! report that caused it, rather than the next tick of a poll.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, watch};

use crate::clock::{self, Clock};
use crate::history::JobTracker;
use crate::p1s::{self, HmsEntry, Link, Snapshot};
use crate::printers::Printers;
use crate::push::{self, Notification, Sender};
use crate::settings::Values;
use crate::timers::{self, Timers};

/// Something the reactor wants done, outside itself.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    SetBedTemp(i64),
    SetNozzleTemp(i64),
    Lamp(bool),
    Notify(Notification),
    /// The bed has been on since this time (unix seconds), holding `target`.
    RemindBed {
        since: i64,
        target: f64,
    },
    /// The bed went off; every device's reminder schedule starts over.
    ForgetBedReminders,
    /// A job row was opened or closed.
    JobsChanged,
}

/// Heater shut-off: a bed or nozzle set hot through this app is turned off a
/// set time later. Setting it again, including to off, resets the countdown;
/// the window is read now, so a later settings change doesn't move it.
pub fn arm_heater(timers: &Timers, name: &str, temp: i64, now: i64, window: i64) {
    if temp > 0 {
        timers.set(name, now + window)
    } else {
        timers.clear(name)
    }
}

pub struct Reactor {
    timers: Timers,
    jobs: JobTracker,
    settings: Arc<dyn Fn() -> Values + Send + Sync>,

    /// Whether a connected snapshot has been seen yet. The first is only
    /// recorded, never announced: a print that finished before a restart is
    /// not news.
    observed: bool,
    was_busy: bool,
    errors_seen: Vec<String>,

    /// The lamp's view: whether it has looked yet, and whether the printer was
    /// active (a print running, or a heater set) when it last did.
    lamp_observed: bool,
    was_active: bool,

    /// Whether devices may hold reminder state that needs forgetting.
    reminding: bool,
}

impl Reactor {
    /// Resumes from the timers pending when the process last stopped.
    pub fn new(
        timers: Timers,
        jobs: JobTracker,
        settings: impl Fn() -> Values + Send + Sync + 'static,
    ) -> Reactor {
        // A pending lamp shut-off means the printer was idle when it was
        // armed, which is also what the first look would conclude. Saying so
        // here keeps that look from re-arming and losing the elapsed time.
        let lamp_observed = timers.get(timers::LAMP_OFF).is_some();
        Reactor {
            timers,
            jobs,
            settings: Arc::new(settings),
            observed: false,
            was_busy: false,
            errors_seen: Vec::new(),
            lamp_observed,
            was_active: false,
            reminding: true,
        }
    }

    /// Applies every rule to the printer's state at `now_ms`.
    pub fn evaluate(&mut self, snap: &Snapshot, now_ms: i64) -> Vec<Effect> {
        let now = clock::secs(now_ms);
        let fields = &snap.fields;
        let state = p1s::gcode_state(fields);
        let busy = p1s::job_active(state);
        let name = p1s::job_name_text(fields);
        let bed_target = p1s::number(fields, "bed_target_temper").unwrap_or(0.0);
        let nozzle_target = p1s::number(fields, "nozzle_target_temper").unwrap_or(0.0);
        let mut out = Vec::new();

        if self.jobs.observe(state, &name, now) {
            out.push(Effect::JobsChanged);
        }
        self.auto_off(snap.connected, state, now, &mut out);

        // A disconnected printer reports nothing rather than its last known
        // state, so a dropped link cannot look like a print ending. Nor does
        // one that has connected but not yet said what it is doing: the
        // moment between connecting and the first report would otherwise read
        // as idle, and the report after it as a print starting.
        if snap.connected && state != "unknown" {
            let first = !self.observed;
            self.observed = true;
            if !first {
                self.job_change(busy, state, &name, &mut out);
            }
            self.new_errors(&p1s::hms_errors(fields), first, &mut out);
            self.track_bed_on(busy, bed_target, now);
            self.was_busy = busy;
            self.lamp(
                busy || bed_target > 0.0 || nozzle_target > 0.0,
                now,
                &mut out,
            );
        }

        match self.timers.get(timers::BED_ON_SINCE) {
            Some(since) => {
                self.reminding = true;
                out.push(Effect::RemindBed {
                    since,
                    target: bed_target,
                });
            }
            None if self.reminding => {
                self.reminding = false;
                out.push(Effect::ForgetBedReminders);
            }
            None => {}
        }
        out
    }

    /// Shuts a heater down once its deadline passes, but only while the printer
    /// is idle: mid-print the printer owns its temperatures, and cutting them
    /// ruins the print. A deadline reached mid-print fires the first moment the
    /// printer is idle again.
    fn auto_off(&self, connected: bool, state: &str, now: i64, out: &mut Vec<Effect>) {
        if p1s::action_allowed(connected, state).is_err() {
            return;
        }
        for (timer, effect, title, tag) in [
            (
                timers::BED_OFF,
                Effect::SetBedTemp(0),
                "Bed turned off",
                push::TAG_BED,
            ),
            (
                timers::NOZZLE_OFF,
                Effect::SetNozzleTemp(0),
                "Nozzle turned off",
                push::TAG_NOZZLE,
            ),
        ] {
            if self.timers.due(timer, now) {
                self.timers.clear(timer);
                out.push(effect);
                out.push(Effect::Notify(Notification::new(
                    title,
                    "It had been on since it was last set here.",
                    tag,
                    push::KIND_HEATER_OFF,
                )));
            }
        }
    }

    fn job_change(&self, busy: bool, state: &str, name: &str, out: &mut Vec<Effect>) {
        let name = if name.is_empty() { "The print" } else { name };
        let (title, kind) = match (busy, self.was_busy, state) {
            (true, false, _) => ("Print started", push::KIND_PRINT_STARTED),
            (false, true, "FINISH") => ("Print finished", push::KIND_PRINT_FINISHED),
            // The printer reports the same state whether it gave up or someone
            // pressed Stop, so the wording must not claim to know which.
            (false, true, "FAILED") => ("Print ended without finishing", push::KIND_PRINT_ENDED),
            _ => return,
        };
        out.push(Effect::Notify(Notification::new(
            title,
            name,
            push::TAG_JOB,
            kind,
        )));
    }

    /// Announces errors that were not standing a moment ago. Filament runout
    /// arrives this way. Forgetting errors that clear is what lets the same
    /// fault announce itself again if it comes back.
    fn new_errors(&mut self, errors: &[HmsEntry], first: bool, out: &mut Vec<Effect>) {
        if !first {
            for e in errors
                .iter()
                .filter(|e| !self.errors_seen.contains(&e.code))
            {
                out.push(Effect::Notify(Notification::new(
                    "Printer error",
                    &e.message,
                    push::TAG_ERROR,
                    push::KIND_PRINTER_ERROR,
                )));
            }
        }
        self.errors_seen = errors.iter().map(|e| e.code.clone()).collect();
    }

    /// Notes when the bed came on with no print running. At startup the bed
    /// may already have been on for hours, which cannot be known — so the
    /// clock starts now and a reminder comes late rather than invented.
    fn track_bed_on(&self, busy: bool, bed_target: f64, now: i64) {
        if bed_target <= 0.0 || busy {
            self.timers.clear(timers::BED_ON_SINCE);
        } else if self.timers.get(timers::BED_ON_SINCE).is_none() {
            self.timers.set(timers::BED_ON_SINCE, now);
        }
    }

    /// Becoming active turns the lamp on once; becoming inactive arms a
    /// countdown that turns it off once. Manual toggles in between are left
    /// alone. The first look counts as a change either way, so a restart
    /// mid-print turns the lamp on and a restart while idle arms the countdown.
    fn lamp(&mut self, active: bool, now: i64, out: &mut Vec<Effect>) {
        let first = !self.lamp_observed;
        self.lamp_observed = true;
        if active {
            self.timers.clear(timers::LAMP_OFF);
            if first || !self.was_active {
                self.was_active = true;
                out.push(Effect::Lamp(true));
            }
            return;
        }
        if first || self.was_active {
            self.was_active = false;
            self.timers
                .set(timers::LAMP_OFF, now + (self.settings)().lamp_off_after);
        }
        if self.timers.due(timers::LAMP_OFF, now) {
            self.timers.clear(timers::LAMP_OFF);
            out.push(Effect::Lamp(false));
        }
    }

    /// Re-evaluates on every state change and whenever a timer or reminder
    /// comes due, forever. Spawn once.
    pub async fn run(mut self, mut state: watch::Receiver<Snapshot>, runner: Runner) {
        loop {
            let snap = state.borrow_and_update().clone();
            let now = (runner.clock)();
            let mut next_reminder = None;
            for effect in self.evaluate(&snap, now) {
                if let Some(due) = runner.apply(effect, now) {
                    next_reminder = Some(due);
                }
            }
            let wake = [self.timers.earliest(), next_reminder]
                .into_iter()
                .flatten()
                .min();
            let sleep =
                wake.map(|at| Duration::from_millis((at * 1000 - (runner.clock)()).max(0) as u64));
            tokio::select! {
                changed = state.changed() => if changed.is_err() { return },
                _ = self.timers.changed() => {}
                _ = async { tokio::time::sleep(sleep.unwrap()).await }, if sleep.is_some() => {}
            }
        }
    }
}

/// Carries out the reactor's effects, for printer `printer`.
#[derive(Clone)]
pub struct Runner {
    pub link: Link,
    pub sender: Sender,
    pub jobs_changed: Arc<Notify>,
    pub clock: Clock,
    pub printer: i64,
    /// For the printer's name, which starts each notification once there is
    /// more than one printer.
    pub printers: Printers,
}

impl Runner {
    /// Does one effect. For a reminder, returns when the next one comes due.
    fn apply(&self, effect: Effect, now_ms: i64) -> Option<i64> {
        match effect {
            Effect::SetBedTemp(t) => self.link.set_bed_temp(t),
            Effect::SetNozzleTemp(t) => self.link.set_nozzle_temp(t),
            Effect::Lamp(on) => self.link.set_chamber_light(on),
            Effect::Notify(mut n) => {
                n.title = format!("{}{}", self.printers.label(self.printer), n.title);
                // So one printer's notification doesn't replace another's.
                n.tag = format!("{}-{}", n.tag, self.printer);
                let sender = self.sender.clone();
                tokio::spawn(async move {
                    if let Err(err) = sender.send(&n).await {
                        tracing::warn!("notify: {:?}: {err}", n.title);
                    }
                });
            }
            Effect::RemindBed { since, target } => {
                return self.sender.remind_bed_on(
                    self.printer,
                    &self.printers.label(self.printer),
                    since,
                    target,
                    clock::secs(now_ms),
                );
            }
            Effect::ForgetBedReminders => {
                if let Err(err) = self.sender.forget_bed_reminders(self.printer) {
                    tracing::warn!("notify: clearing bed reminders: {err}");
                }
            }
            Effect::JobsChanged => self.jobs_changed.notify_one(),
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::history::Store;
    use serde_json::{Value, json};

    const T0: i64 = 1_000_000;

    struct Fixture {
        reactor: Reactor,
        timers: Timers,
        store: Store,
        db: Db,
    }

    fn fixture() -> Fixture {
        fixture_on(Db::memory())
    }

    fn fixture_on(db: Db) -> Fixture {
        let timers = Timers::new(db.clone()).unwrap();
        let store = Store::new(db.clone());
        let reactor = Reactor::new(
            timers.clone(),
            JobTracker::new(store.clone()),
            Values::default,
        );
        Fixture {
            reactor,
            timers,
            store,
            db,
        }
    }

    fn snap(connected: bool, fields: Value) -> Snapshot {
        Snapshot {
            fields: Arc::new(fields.as_object().unwrap().clone()),
            connected,
            problem: None,
        }
    }

    fn printer(state: &str, name: &str) -> Snapshot {
        snap(true, json!({"gcode_state": state, "subtask_name": name}))
    }

    impl Fixture {
        fn at(&mut self, secs: i64, s: &Snapshot) -> Vec<Effect> {
            self.reactor.evaluate(s, (T0 + secs) * 1000)
        }
    }

    fn titles(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Notify(n) => Some(n.title.clone()),
                _ => None,
            })
            .collect()
    }

    fn has(effects: &[Effect], want: &Effect) -> bool {
        effects.contains(want)
    }

    // Print notifications.

    #[test]
    fn the_first_look_never_notifies() {
        for state in ["RUNNING", "FINISH", "FAILED", "IDLE"] {
            let mut f = fixture();
            assert!(
                titles(&f.at(0, &printer(state, "benchy.gcode"))).is_empty(),
                "{state}"
            );
        }
    }

    #[test]
    fn print_started_finished_and_ended() {
        for (end, title, kind) in [
            ("FINISH", "Print finished", push::KIND_PRINT_FINISHED),
            (
                "FAILED",
                "Print ended without finishing",
                push::KIND_PRINT_ENDED,
            ),
        ] {
            let mut f = fixture();
            f.at(0, &printer("IDLE", ""));
            let started = f.at(1, &printer("RUNNING", "benchy.gcode"));
            assert!(has(
                &started,
                &Effect::Notify(Notification::new(
                    "Print started",
                    "benchy.gcode",
                    push::TAG_JOB,
                    push::KIND_PRINT_STARTED
                ))
            ));
            assert!(titles(&f.at(2, &printer("RUNNING", "benchy.gcode"))).is_empty());
            let ended = f.at(3, &printer(end, "benchy.gcode"));
            assert!(has(
                &ended,
                &Effect::Notify(Notification::new(
                    title,
                    "benchy.gcode",
                    push::TAG_JOB,
                    kind
                ))
            ));
        }
    }

    #[test]
    fn a_print_going_idle_is_not_announced() {
        let mut f = fixture();
        f.at(0, &printer("IDLE", ""));
        f.at(1, &printer("RUNNING", "a"));
        assert!(titles(&f.at(2, &printer("IDLE", "a"))).is_empty());
    }

    #[test]
    fn pausing_and_resuming_is_not_a_job_boundary() {
        let mut f = fixture();
        f.at(0, &printer("RUNNING", "a"));
        assert!(titles(&f.at(1, &printer("PAUSE", "a"))).is_empty());
        assert!(titles(&f.at(2, &printer("RUNNING", "a"))).is_empty());
    }

    #[test]
    fn a_print_with_no_name_still_reads() {
        let mut f = fixture();
        f.at(0, &printer("IDLE", ""));
        let got = f.at(1, &snap(true, json!({"gcode_state": "RUNNING"})));
        assert!(
            got.iter()
                .any(|e| matches!(e, Effect::Notify(n) if n.body == "The print"))
        );
    }

    #[test]
    fn a_disconnected_printer_reports_nothing() {
        let mut f = fixture();
        f.at(0, &printer("RUNNING", "a"));
        let mut gone = printer("FINISH", "a");
        gone.connected = false;
        assert!(titles(&f.at(1, &gone)).is_empty());
        // Back, still running: nothing ended.
        assert!(titles(&f.at(2, &printer("RUNNING", "a"))).is_empty());
    }

    fn with_errors(codes: &[(u32, u32)]) -> Snapshot {
        let hms: Vec<Value> = codes
            .iter()
            .map(|(a, c)| json!({"attr": a, "code": c}))
            .collect();
        snap(true, json!({"gcode_state": "IDLE", "hms": hms}))
    }

    #[test]
    fn only_new_errors_are_announced_and_a_cleared_one_can_return() {
        let mut f = fixture();
        f.at(0, &with_errors(&[]));
        assert_eq!(
            titles(&f.at(1, &with_errors(&[(0x03008000, 0x00030002)]))),
            ["Printer error"]
        );
        assert!(titles(&f.at(2, &with_errors(&[(0x03008000, 0x00030002)]))).is_empty());
        assert_eq!(
            titles(&f.at(3, &with_errors(&[(0x03008000, 0x00030002), (1, 2)]))),
            ["Printer error"]
        );
        f.at(4, &with_errors(&[]));
        assert_eq!(titles(&f.at(5, &with_errors(&[(1, 2)]))), ["Printer error"]);
        let body = f.at(6, &with_errors(&[(1, 2), (0x03008000, 0x00030002)]));
        assert!(has(
            &body,
            &Effect::Notify(Notification::new(
                "Printer error",
                "AMS filament runout",
                push::TAG_ERROR,
                push::KIND_PRINTER_ERROR
            ))
        ));
    }

    #[test]
    fn connecting_before_the_first_report_is_not_a_change() {
        let mut f = fixture();
        let effects = f.at(0, &snap(true, json!({})));
        assert!(
            effects
                .iter()
                .all(|e| !matches!(e, Effect::Lamp(_) | Effect::Notify(_)))
        );
        let running = f.at(1, &printer("RUNNING", "a"));
        assert!(
            titles(&running).is_empty(),
            "the first report is the first look"
        );
        assert!(has(&running, &Effect::Lamp(true)));
        assert_eq!(f.timers.get(timers::LAMP_OFF), None);
    }

    #[test]
    fn errors_present_at_startup_are_not_announced() {
        let mut f = fixture();
        assert!(titles(&f.at(0, &with_errors(&[(1, 2)]))).is_empty());
    }

    // The bed-on clock and reminders.

    fn bed(state: &str, target: f64) -> Snapshot {
        snap(
            true,
            json!({"gcode_state": state, "bed_target_temper": target}),
        )
    }

    #[test]
    fn the_bed_on_clock_starts_and_stops() {
        let mut f = fixture();
        let first = f.at(0, &bed("IDLE", 0.0));
        assert!(
            has(&first, &Effect::ForgetBedReminders),
            "stale reminder state is cleared at startup"
        );
        assert!(
            !has(&f.at(1, &bed("IDLE", 0.0)), &Effect::ForgetBedReminders),
            "and only once"
        );
        let on = f.at(10, &bed("IDLE", 60.0));
        assert!(has(
            &on,
            &Effect::RemindBed {
                since: T0 + 10,
                target: 60.0
            }
        ));
        let later = f.at(500, &bed("IDLE", 60.0));
        assert!(
            has(
                &later,
                &Effect::RemindBed {
                    since: T0 + 10,
                    target: 60.0
                }
            ),
            "the clock keeps its start"
        );
        let off = f.at(600, &bed("IDLE", 0.0));
        assert!(has(&off, &Effect::ForgetBedReminders));
        assert_eq!(f.timers.get(timers::BED_ON_SINCE), None);
    }

    #[test]
    fn the_bed_on_clock_ignores_a_print() {
        let mut f = fixture();
        f.at(0, &bed("RUNNING", 60.0));
        assert_eq!(f.timers.get(timers::BED_ON_SINCE), None);
    }

    #[test]
    fn the_bed_on_clock_survives_a_restart() {
        let db = Db::memory();
        let mut f = fixture_on(db.clone());
        f.at(0, &bed("IDLE", 60.0));
        let mut again = fixture_on(db);
        assert!(has(
            &again.at(3600, &bed("IDLE", 60.0)),
            &Effect::RemindBed {
                since: T0,
                target: 60.0
            }
        ));
    }

    // Heater auto-off.

    fn armed(f: &Fixture, temp: i64) {
        arm_heater(&f.timers, timers::BED_OFF, temp, T0, 3600);
    }

    #[test]
    fn auto_off_fires_after_its_window_once() {
        let mut f = fixture();
        armed(&f, 60);
        assert!(!has(
            &f.at(3599, &bed("IDLE", 60.0)),
            &Effect::SetBedTemp(0)
        ));
        let fired = f.at(3600, &bed("IDLE", 60.0));
        assert!(has(&fired, &Effect::SetBedTemp(0)));
        assert_eq!(titles(&fired), ["Bed turned off"]);
        assert!(!has(
            &f.at(3601, &bed("IDLE", 60.0)),
            &Effect::SetBedTemp(0)
        ));
        assert_eq!(f.timers.get(timers::BED_OFF), None);
    }

    #[test]
    fn auto_off_resets_and_cancels() {
        let f = fixture();
        armed(&f, 60);
        arm_heater(&f.timers, timers::BED_OFF, 70, T0 + 100, 3600);
        assert_eq!(f.timers.get(timers::BED_OFF), Some(T0 + 3700));
        armed(&f, 0);
        assert_eq!(f.timers.get(timers::BED_OFF), None);
    }

    #[test]
    fn auto_off_waits_for_the_print_and_the_connection() {
        let mut f = fixture();
        arm_heater(&f.timers, timers::NOZZLE_OFF, 220, T0, 60);
        assert!(!has(
            &f.at(100, &bed("RUNNING", 60.0)),
            &Effect::SetNozzleTemp(0)
        ));
        let mut away = bed("FINISH", 60.0);
        away.connected = false;
        assert!(!has(&f.at(200, &away), &Effect::SetNozzleTemp(0)));
        assert!(f.timers.get(timers::NOZZLE_OFF).is_some(), "still pending");
        let back = f.at(300, &bed("FINISH", 60.0));
        assert!(has(&back, &Effect::SetNozzleTemp(0)));
        assert!(titles(&back).contains(&"Nozzle turned off".to_string()));
    }

    #[test]
    fn a_shut_off_that_lapsed_while_down_fires_on_start() {
        let db = Db::memory();
        let f = fixture_on(db.clone());
        armed(&f, 60);
        let mut again = fixture_on(db);
        assert!(has(
            &again.at(7200, &bed("IDLE", 60.0)),
            &Effect::SetBedTemp(0)
        ));
    }

    // The lamp.

    fn lamp(active: bool) -> Snapshot {
        if active {
            printer("RUNNING", "a")
        } else {
            printer("IDLE", "")
        }
    }

    #[test]
    fn lamp_first_look_turns_it_on_if_active() {
        let mut f = fixture();
        assert!(has(&f.at(0, &lamp(true)), &Effect::Lamp(true)));
        assert!(
            !has(&f.at(1, &lamp(true)), &Effect::Lamp(true)),
            "a manual off is not fought"
        );
    }

    #[test]
    fn lamp_first_look_arms_the_countdown_if_idle() {
        let mut f = fixture();
        assert!(
            f.at(0, &lamp(false))
                .iter()
                .all(|e| !matches!(e, Effect::Lamp(_)))
        );
        assert_eq!(f.timers.get(timers::LAMP_OFF), Some(T0 + 8 * 3600));
    }

    #[test]
    fn lamp_turns_off_once_after_the_delay() {
        let mut f = fixture();
        f.at(0, &lamp(true));
        f.at(10, &lamp(false));
        assert_eq!(f.timers.get(timers::LAMP_OFF), Some(T0 + 10 + 8 * 3600));
        assert!(!has(
            &f.at(10 + 8 * 3600 - 1, &lamp(false)),
            &Effect::Lamp(false)
        ));
        assert!(has(
            &f.at(10 + 8 * 3600, &lamp(false)),
            &Effect::Lamp(false)
        ));
        assert!(!has(
            &f.at(10 + 8 * 3600 + 1, &lamp(false)),
            &Effect::Lamp(false)
        ));
    }

    #[test]
    fn lamp_countdown_is_cancelled_by_reactivation() {
        let mut f = fixture();
        f.at(0, &lamp(false));
        assert!(
            has(&f.at(10, &bed("IDLE", 60.0)), &Effect::Lamp(true)),
            "a heater set counts as active"
        );
        assert_eq!(f.timers.get(timers::LAMP_OFF), None);
    }

    #[test]
    fn lamp_does_nothing_while_disconnected() {
        let mut f = fixture();
        let mut s = lamp(true);
        s.connected = false;
        assert!(!has(&f.at(0, &s), &Effect::Lamp(true)));
    }

    #[test]
    fn the_lamp_countdown_survives_a_restart() {
        let db = Db::memory();
        let mut f = fixture_on(db.clone());
        f.at(0, &lamp(false));
        let mut again = fixture_on(db);
        again.at(100, &lamp(false));
        assert_eq!(
            again.timers.get(timers::LAMP_OFF),
            Some(T0 + 8 * 3600),
            "not re-armed"
        );
    }

    // Job rows.

    #[test]
    fn job_rows_follow_the_print() {
        let mut f = fixture();
        assert!(has(
            &f.at(0, &printer("RUNNING", "a")),
            &Effect::JobsChanged
        ));
        assert!(!has(
            &f.at(1, &printer("RUNNING", "a")),
            &Effect::JobsChanged
        ));
        assert!(has(&f.at(2, &printer("FINISH", "a")), &Effect::JobsChanged));
        let jobs = f.store.recent_jobs().unwrap();
        assert_eq!((jobs[0].start, jobs[0].end), (T0, Some(T0 + 2)));
        let _ = &f.db;
    }
}
