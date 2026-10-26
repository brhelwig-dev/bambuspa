use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use bambuspa_api::{Event, Events};

use super::{App, Query, json_reply, text};
use crate::settings::{self as keys, BYTES_PER_MB};

const SHOWN_EVENTS: usize = 500;

pub async fn get(State(app): State<App>) -> Response {
    let v = app.settings.values();
    json_reply(&bambuspa_api::Settings {
        timelapse_storage: v.timelapse_storage / BYTES_PER_MB,
        bed_off_after: v.bed_off_after,
        nozzle_off_after: v.nozzle_off_after,
        lamp_off_after: v.lamp_off_after,
        activity_limit: v.activity_limit / BYTES_PER_MB,
        session_length: v.session_length,
        dashboard: v.dashboard,
    })
}

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

pub async fn events(State(app): State<App>) -> Response {
    let events = app.activity.entries(SHOWN_EVENTS);
    json_reply(&Events {
        events: events.iter().map(Event::from).collect(),
    })
}

use axum::response::IntoResponse;
