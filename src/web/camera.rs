//! The recorded camera footage.

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::{App, Query, json_reply, text};
use crate::p1s;
use crate::printers::Printer;
use std::sync::Arc;

/// How far before a running print's start the scrub bar begins, so the whole
/// job is one drag of the bar with nothing earlier in the way.
pub const JOB_LEAD_IN: i64 = 5 * 60;

/// The unix second a served frame was actually captured: the first at or after
/// the one requested, which gaps in the footage can make hours later.
pub const FRAME_TIMESTAMP: &str = "X-Frame-Timestamp";

/// The printer a camera request is about. With none, an empty answer.
fn pick(app: &App, query: &Query) -> Result<Arc<Printer>, Box<Response>> {
    match app.printer(query)? {
        Some(p) => Ok(p),
        None => Err(Box::new(text(StatusCode::NOT_FOUND, "no printer set up"))),
    }
}

pub async fn range(State(app): State<App>, query: Query) -> Response {
    let printer = match app.printer(&query) {
        Ok(Some(p)) => p,
        Ok(None) => return json_reply(&json!({"oldest": null, "newest": null})),
        Err(refusal) => return *refusal,
    };
    match range_of(&printer) {
        Ok(v) => json_reply(&v),
        Err(_) => text(StatusCode::INTERNAL_SERVER_ERROR, "range query failed"),
    }
}

/// The span the scrub bar covers: everything stored, or just the running
/// print while there is one. Earlier prints are reached by picking a job.
pub fn range_of(printer: &Printer) -> rusqlite::Result<Value> {
    let (mut oldest, newest) = printer.store.range()?;
    if let (Some(old), Some(new), Some(start)) = (oldest, newest, running_job_start(printer))
        && start > old
    {
        // A print that started after the last recorded frame (camera down
        // when it began) would otherwise put the start past the end.
        oldest = Some(start.min(new));
    }
    Ok(json!({"oldest": oldest, "newest": newest}))
}

/// Just before the running print began, if one is running.
fn running_job_start(printer: &Printer) -> Option<i64> {
    let snap = printer.cache.snapshot();
    if !p1s::job_active(p1s::gcode_state(&snap.fields)) {
        return None;
    }
    let job = printer.store.active_job().ok()??;
    Some(job.start - JOB_LEAD_IN)
}

pub async fn frame(State(app): State<App>, query: Query) -> Response {
    let Some(ts) = query.get("ts").and_then(|t| t.parse::<i64>().ok()) else {
        return text(StatusCode::BAD_REQUEST, "invalid ts");
    };
    let printer = match pick(&app, &query) {
        Ok(p) => p,
        Err(refusal) => return *refusal,
    };
    match printer.store.frame_at_or_after(ts) {
        Ok(Some((jpeg, taken))) => {
            let mut resp = jpeg.into_response();
            let headers = resp.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("image/jpeg"),
            );
            headers.insert(FRAME_TIMESTAMP, taken.into());
            resp
        }
        Ok(None) => text(StatusCode::NOT_FOUND, "no frame at or after ts"),
        Err(_) => text(StatusCode::INTERNAL_SERVER_ERROR, "frame query failed"),
    }
}

pub async fn jobs(State(app): State<App>, query: Query) -> Response {
    let printer = match app.printer(&query) {
        Ok(Some(p)) => p,
        Ok(None) => return json_reply(&json!([])),
        Err(refusal) => return *refusal,
    };
    match printer.store.recent_jobs() {
        Ok(jobs) => json_reply(&jobs),
        Err(_) => text(StatusCode::INTERNAL_SERVER_ERROR, "jobs query failed"),
    }
}
