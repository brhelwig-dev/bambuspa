//! POST /api/actions/{name}: the printer controls.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;

use super::{App, Query, text};
use crate::core::arm_heater;
use crate::p1s;
use crate::printers::Printer;
use crate::timers;

/// The P1S bed tops out near 100°C and the nozzle near 300°C; the small
/// headroom just guards the top preset against rounding.
pub const BED_MAX_TEMP: i64 = 110;
pub const NOZZLE_MAX_TEMP: i64 = 300;
/// Pushing filament through a cold nozzle strips it and can jam the extruder.
/// The firmware blocks cold extrusion too; this matches it defensively.
pub const EXTRUDE_MIN_TEMP: f64 = 170.0;
/// Bambu supports up to four AMS units, addressed 0-3.
const MAX_AMS_UNIT: i64 = 3;

fn blocked(reason: String) -> Response {
    text(StatusCode::CONFLICT, format!("blocked: {reason}"))
}

fn bad(msg: impl Into<String>) -> Response {
    text(StatusCode::BAD_REQUEST, msg)
}

fn sent(what: String) -> Response {
    text(StatusCode::OK, format!("sent: {what}"))
}

fn parse_temp(raw: &str, max: i64) -> Result<i64, String> {
    let t: i64 = raw.parse().map_err(|_| format!("invalid temp {raw:?}"))?;
    if !(0..=max).contains(&t) {
        return Err(format!("temp {t} out of range 0-{max}"));
    }
    Ok(t)
}

/// Every action there is.
const ACTIONS: &[&str] = &[
    "set-bed-temp",
    "set-nozzle-temp",
    "extrude",
    "set-filament",
    "lamp-on",
    "lamp-off",
    "lower-bed",
    "home",
    "unload",
    "pause",
    "resume",
    "stop",
];

pub async fn action(State(app): State<App>, Path(name): Path<String>, query: Query) -> Response {
    if !ACTIONS.contains(&name.as_str()) {
        return text(StatusCode::NOT_FOUND, "unknown action");
    }
    let printer = match app.printer(&query) {
        Ok(Some(p)) => p,
        Ok(None) => return blocked("no printer set up".into()),
        Err(refusal) => return *refusal,
    };
    let p = &*printer;
    let snap = p.cache.snapshot();
    let (connected, state) = (snap.connected, p1s::gcode_state(&snap.fields));
    let arg = |key: &str| query.get(key).map(String::as_str).unwrap_or("");
    // A refusal when bed, temperature and filament actions may not run.
    let not_idle = || p1s::action_allowed(connected, state).err().map(blocked);

    match name.as_str() {
        // The sliders post an arbitrary target. Each set (re)arms or cancels
        // the heater's automatic shut-off.
        "set-bed-temp" | "set-nozzle-temp" => {
            let bed = name == "set-bed-temp";
            let temp = match parse_temp(
                arg("temp"),
                if bed { BED_MAX_TEMP } else { NOZZLE_MAX_TEMP },
            ) {
                Ok(t) => t,
                Err(err) => return bad(err),
            };
            if let Some(refusal) = not_idle() {
                return refusal;
            }
            let v = app.settings.values();
            let now = crate::clock::secs((app.clock)());
            if bed {
                p.link.set_bed_temp(temp);
                arm_heater(&p.timers, timers::BED_OFF, temp, now, v.bed_off_after);
            } else {
                p.link.set_nozzle_temp(temp);
                arm_heater(&p.timers, timers::NOZZLE_OFF, temp, now, v.nozzle_off_after);
            }
            sent(format!("{name} {temp}"))
        }
        "extrude" => {
            if let Some(refusal) = not_idle() {
                return refusal;
            }
            if p1s::number(&snap.fields, "nozzle_temper").is_none_or(|t| t < EXTRUDE_MIN_TEMP) {
                return blocked(format!("nozzle below {EXTRUDE_MIN_TEMP:.0}°C"));
            }
            p.link.extrude();
            sent(name)
        }
        "set-filament" => set_filament(p, &query).unwrap_or_else(|refusal| *refusal),
        // The lamp is safe in any state; it only needs the printer reachable.
        "lamp-on" | "lamp-off" => {
            if !connected {
                return blocked("not connected to printer".into());
            }
            p.link.set_chamber_light(name == "lamp-on");
            sent(name)
        }
        "lower-bed" | "home" | "unload" => {
            if let Some(refusal) = not_idle() {
                return refusal;
            }
            match name.as_str() {
                "lower-bed" => p.link.lower_bed(),
                "home" => p.link.home(),
                _ => p.link.unload_filament(),
            }
            sent(name)
        }
        "pause" | "resume" | "stop" => {
            if let Err(reason) = p1s::print_action_allowed(connected, state, &name) {
                return blocked(reason);
            }
            match name.as_str() {
                "pause" => p.link.pause(),
                "resume" => p.link.resume(),
                _ => p.link.stop_print(),
            }
            sent(name)
        }
        _ => text(StatusCode::NOT_FOUND, "unknown action"),
    }
}

/// Writes a tray's filament profile. A full-tray write, so the page resends the
/// tray's existing type, temperatures and profile id alongside the field it
/// changed — otherwise the printer would blank them. Everything is checked
/// before the printer's state is.
fn set_filament(p: &Printer, query: &Query) -> Result<Response, Box<Response>> {
    let arg = |key: &str| query.get(key).map(String::as_str).unwrap_or("");
    let refuse = |msg: &str| Box::new(bad(msg));
    let index = |raw: &str, max: i64| raw.parse::<i64>().ok().filter(|n| (0..=max).contains(n));
    let ams_id = index(arg("ams_id"), MAX_AMS_UNIT).ok_or_else(|| refuse("invalid ams_id"))?;
    let tray_id = index(arg("tray_id"), 3).ok_or_else(|| refuse("invalid tray_id"))?;
    let color = normalize_color(arg("tray_color")).map_err(|e| refuse(&e))?;
    let kind = arg("tray_type");
    if kind.is_empty() || kind.len() > 32 {
        return Err(refuse("invalid tray_type"));
    }
    let min = parse_temp(arg("nozzle_temp_min"), NOZZLE_MAX_TEMP)
        .map_err(|_| refuse("invalid nozzle_temp_min"))?;
    let max = parse_temp(arg("nozzle_temp_max"), NOZZLE_MAX_TEMP)
        .map_err(|_| refuse("invalid nozzle_temp_max"))?;
    if min > max {
        return Err(refuse("nozzle_temp_min above nozzle_temp_max"));
    }
    let info_idx = arg("tray_info_idx");
    if info_idx.len() > 32 {
        return Err(refuse("invalid tray_info_idx"));
    }
    let snap = p.cache.snapshot();
    p1s::action_allowed(snap.connected, p1s::gcode_state(&snap.fields))
        .map_err(|r| Box::new(blocked(r)))?;
    p.link
        .set_ams_filament(ams_id, tray_id, info_idx, &color, kind, min, max);
    Ok(sent(format!("set-filament ams {ams_id} tray {tray_id}")))
}

/// A 6- or 8-digit hex colour as uppercase RRGGBBAA (alpha FF when only RRGGBB
/// is given), the form the AMS reports and expects.
pub fn normalize_color(raw: &str) -> Result<String, String> {
    let mut up = raw.to_uppercase();
    if up.len() == 6 {
        up.push_str("FF");
    }
    if up.len() != 8 {
        return Err("tray_color must be RRGGBB or RRGGBBAA hex".into());
    }
    if !up.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("tray_color must be hex".into());
    }
    Ok(up)
}
