//! The Bambu P1-series local protocols: MQTT state and commands on :8883, and
//! the chamber-camera stream on :6000.

mod camera;
mod hms;
mod link;
pub mod mqtt;
mod state;
mod tls;

pub use camera::stream_frames;
pub use hms::{HmsEntry, hms_errors};
pub use link::{Config, Link, Ports};
pub use state::{Snapshot, StateCache};

use serde_json::{Map, Value};

/// The printer's merged report fields.
pub type Fields = Map<String, Value>;

/// States in which it is safe to move the bed or change temperatures.
/// Anything else (RUNNING, PREPARE, PAUSE, unknown) blocks those actions.
const IDLE_STATES: &[&str] = &["IDLE", "FINISH", "FAILED"];

pub fn gcode_state(fields: &Fields) -> &str {
    fields
        .get("gcode_state")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

/// Whether a print is under way. A paused print is still the same job.
pub fn job_active(gcode_state: &str) -> bool {
    gcode_state == "RUNNING" || gcode_state == "PAUSE"
}

/// Whether the printer is in a state that means no print is under way.
/// Deliberately not the negation of `job_active`: "unknown" (before the
/// printer's first report) and PREPARE are neither, and a caller that acts on a
/// job ending must not act on the absence of information.
pub fn job_ended(gcode_state: &str) -> bool {
    IDLE_STATES.contains(&gcode_state)
}

/// Whether bed, temperature and filament actions may run.
pub fn action_allowed(connected: bool, gcode_state: &str) -> Result<(), String> {
    if !connected {
        return Err("not connected to printer".into());
    }
    if !job_ended(gcode_state) {
        return Err(format!("printer state is {gcode_state}"));
    }
    Ok(())
}

/// Guards the print-flow controls, which only mean something mid-print.
pub fn print_action_allowed(
    connected: bool,
    gcode_state: &str,
    action: &str,
) -> Result<(), String> {
    if !connected {
        return Err("not connected to printer".into());
    }
    let ok = match action {
        "pause" => gcode_state == "RUNNING",
        "resume" => gcode_state == "PAUSE",
        "stop" => job_active(gcode_state),
        _ => return Err(format!("unknown print action {action}")),
    };
    if ok {
        return Ok(());
    }
    Err(match action {
        "pause" => format!("can only pause while RUNNING, printer state is {gcode_state}"),
        "resume" => format!("can only resume while PAUSE, printer state is {gcode_state}"),
        _ => format!("can only stop while RUNNING or PAUSE, printer state is {gcode_state}"),
    })
}

/// The active job's display name, as the printer sent it. "subtask_name" is
/// the name Bambu Studio/Handy assigns; "gcode_file" (the raw filename) is the
/// fallback for jobs without one, e.g. local SD prints.
pub fn job_name(fields: &Fields) -> Value {
    match fields.get("subtask_name") {
        Some(Value::String(name)) if !name.is_empty() => Value::String(name.clone()),
        _ => fields.get("gcode_file").cloned().unwrap_or(Value::Null),
    }
}

/// The job name as text, or empty when there is none.
pub fn job_name_text(fields: &Fields) -> String {
    match job_name(fields) {
        Value::String(name) => name,
        _ => String::new(),
    }
}

/// The chamber lamp's state from the printer's "lights_report" array. None
/// means it has not been reported yet, so the page won't assert a state it
/// hasn't actually seen.
pub fn chamber_light(fields: &Fields) -> Option<bool> {
    fields
        .get("lights_report")?
        .as_array()?
        .iter()
        .find(|item| item.get("node").and_then(Value::as_str) == Some("chamber_light"))
        .map(|item| item.get("mode").and_then(Value::as_str) == Some("on"))
}

/// A numeric field, or None when it is missing or not a number.
pub fn number(fields: &Fields, name: &str) -> Option<f64> {
    fields.get(name).and_then(Value::as_f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(v: Value) -> Fields {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn action_guard() {
        assert_eq!(
            action_allowed(false, "IDLE").unwrap_err(),
            "not connected to printer"
        );
        for s in ["IDLE", "FINISH", "FAILED"] {
            assert!(action_allowed(true, s).is_ok());
        }
        for s in ["RUNNING", "PAUSE", "PREPARE", "unknown"] {
            assert_eq!(
                action_allowed(true, s).unwrap_err(),
                format!("printer state is {s}")
            );
        }
    }

    #[test]
    fn gcode_state_defaults_to_unknown() {
        assert_eq!(gcode_state(&fields(json!({}))), "unknown");
        assert_eq!(gcode_state(&fields(json!({"gcode_state": 3}))), "unknown");
        assert_eq!(
            gcode_state(&fields(json!({"gcode_state": "RUNNING"}))),
            "RUNNING"
        );
    }

    #[test]
    fn job_active_and_ended() {
        let table = [
            ("RUNNING", true, false),
            ("PAUSE", true, false),
            ("IDLE", false, true),
            ("FINISH", false, true),
            ("FAILED", false, true),
            ("PREPARE", false, false),
            ("unknown", false, false),
        ];
        for (state, active, ended) in table {
            assert_eq!(
                (job_active(state), job_ended(state)),
                (active, ended),
                "{state}"
            );
        }
    }

    #[test]
    fn print_action_guard() {
        assert!(print_action_allowed(true, "RUNNING", "pause").is_ok());
        assert!(print_action_allowed(true, "PAUSE", "resume").is_ok());
        assert!(print_action_allowed(true, "RUNNING", "stop").is_ok());
        assert!(print_action_allowed(true, "PAUSE", "stop").is_ok());
        assert_eq!(
            print_action_allowed(true, "IDLE", "pause").unwrap_err(),
            "can only pause while RUNNING, printer state is IDLE"
        );
        assert_eq!(
            print_action_allowed(true, "RUNNING", "resume").unwrap_err(),
            "can only resume while PAUSE, printer state is RUNNING"
        );
        assert_eq!(
            print_action_allowed(true, "FINISH", "stop").unwrap_err(),
            "can only stop while RUNNING or PAUSE, printer state is FINISH"
        );
        assert_eq!(
            print_action_allowed(false, "RUNNING", "stop").unwrap_err(),
            "not connected to printer"
        );
        assert!(print_action_allowed(true, "RUNNING", "nope").is_err());
    }

    #[test]
    fn job_name_prefers_the_subtask_name() {
        assert_eq!(
            job_name(&fields(
                json!({"subtask_name": "Benchy", "gcode_file": "a.gcode"})
            )),
            json!("Benchy")
        );
        assert_eq!(
            job_name(&fields(
                json!({"subtask_name": "", "gcode_file": "a.gcode"})
            )),
            json!("a.gcode")
        );
        assert_eq!(job_name(&fields(json!({}))), Value::Null);
        assert_eq!(job_name_text(&fields(json!({"gcode_file": 4}))), "");
    }

    #[test]
    fn chamber_light_is_three_valued() {
        let on = json!({"lights_report": [{"node": "work_light", "mode": "off"}, {"node": "chamber_light", "mode": "on"}]});
        assert_eq!(chamber_light(&fields(on)), Some(true));
        let flashing = json!({"lights_report": [{"node": "chamber_light", "mode": "flashing"}]});
        assert_eq!(chamber_light(&fields(flashing)), Some(false));
        assert_eq!(chamber_light(&fields(json!({"lights_report": []}))), None);
        assert_eq!(chamber_light(&fields(json!({}))), None);
    }
}
