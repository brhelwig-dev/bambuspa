//! The Settings and Events screens.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;

use super::{App, Query, json_reply, text};
use crate::settings::{self as keys, BYTES_PER_MB};

/// How many entries the Events screen is sent.
const SHOWN_EVENTS: usize = 500;

/// The current values in their stored units: seconds, megabytes or a count,
/// plus the dashboard layout as text. The printer's details are not here.
pub async fn get(State(app): State<App>) -> Response {
    let v = app.settings.values();
    json_reply(&json!({
        keys::CAMERA_STORAGE: v.camera_storage / BYTES_PER_MB,
        keys::BED_OFF_AFTER: v.bed_off_after,
        keys::NOZZLE_OFF_AFTER: v.nozzle_off_after,
        keys::LAMP_OFF_AFTER: v.lamp_off_after,
        keys::ACTIVITY_LIMIT: v.activity_limit / BYTES_PER_MB,
        keys::SESSION_LENGTH: v.session_length,
        keys::DASHBOARD: v.dashboard,
    }))
}

/// Stores one value. Nothing has to be told: everything that consults a
/// setting does so at the point of use.
pub async fn set(State(app): State<App>, Path(name): Path<String>, query: Query) -> Response {
    if keys::is_text(&name) {
        if name != keys::DASHBOARD {
            return text(StatusCode::BAD_REQUEST, "not writable here");
        }
        let value = query.get("text").map(String::as_str).unwrap_or("");
        return match app.settings.set_text(&name, value) {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(err) => text(StatusCode::BAD_REQUEST, err),
        };
    }
    let Some(value) = query.get("value").and_then(|v| v.parse::<i64>().ok()) else {
        return text(StatusCode::BAD_REQUEST, "invalid value");
    };
    match app.settings.set(&name, value) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => text(StatusCode::BAD_REQUEST, err),
    }
}

/// The most recent event-log entries, newest first.
pub async fn events(State(app): State<App>) -> Response {
    json_reply(&json!({"events": app.activity.entries(SHOWN_EVENTS)}))
}

use axum::response::IntoResponse;
