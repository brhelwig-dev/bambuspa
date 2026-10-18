use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bambuspa_api::{FRAME_TIMESTAMP, Job, Range};

use super::{App, Query, json_reply, text};
use crate::p1s;
use crate::printers::Printer;
use std::sync::Arc;

pub const JOB_LEAD_IN: i64 = 5 * 60;

fn pick(app: &App, query: &Query) -> Result<Arc<Printer>, Box<Response>> {
    match app.printer(query)? {
        Some(p) => Ok(p),
        None => Err(Box::new(text(StatusCode::NOT_FOUND, "no printer set up"))),
    }
}

pub async fn range(State(app): State<App>, query: Query) -> Response {
    let printer = match app.printer(&query) {
        Ok(Some(p)) => p,
        Ok(None) => return json_reply(&Range::default()),
        Err(refusal) => return *refusal,
    };
    match range_of(&printer) {
        Ok(v) => json_reply(&v),
        Err(_) => text(StatusCode::INTERNAL_SERVER_ERROR, "range query failed"),
    }
}

pub fn range_of(printer: &Printer) -> rusqlite::Result<Range> {
    let (mut oldest, newest) = printer.store.range()?;
    if let (Some(old), Some(new), Some(start)) = (oldest, newest, running_job_start(printer))
        && start > old
    {
        oldest = Some(start.min(new));
    }
    Ok(Range { oldest, newest })
}

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
        Ok(None) => return json_reply(&Vec::<Job>::new()),
        Err(refusal) => return *refusal,
    };
    match printer.store.recent_jobs() {
        Ok(jobs) => json_reply(&jobs),
        Err(_) => text(StatusCode::INTERNAL_SERVER_ERROR, "jobs query failed"),
    }
}
