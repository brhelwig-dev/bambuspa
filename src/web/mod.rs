//! The phone-facing HTTP interface.

mod actions;
mod camera;
mod csrf;
mod live;
mod notify;
mod printers;
mod settings;
mod statics;

use std::collections::HashMap;
use std::sync::Arc;

use crate::activity::Log;
use crate::clock::Clock;
use crate::p1s::{self, Snapshot};
use crate::printers::{Printer, Printers};
use crate::push::Sender;
use crate::settings::Settings;
use crate::timers;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;
use serde_json::{Value, json};

pub use csrf::CrossOrigin;
pub use live::Feed;

/// Everything the handlers reach.
#[derive(Clone)]
pub struct App {
    pub printers: Printers,
    pub sender: Sender,
    pub settings: Settings,
    pub activity: Log,
    pub cross_origin: CrossOrigin,
    pub clock: Clock,
}

impl App {
    /// The printer a request is about: `?printer=<id>`, or the first one when
    /// it doesn't say. None when there are no printers and none was named.
    fn printer(&self, query: &Query) -> Result<Option<Arc<Printer>>, Box<Response>> {
        let Some(raw) = query.get("printer") else {
            return Ok(self.printers.first());
        };
        raw.parse::<i64>()
            .ok()
            .and_then(|id| self.printers.get(id))
            .map(Some)
            .ok_or_else(|| Box::new(text(StatusCode::NOT_FOUND, "no such printer")))
    }
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/actions/{name}", post(actions::action))
        .route("/api/live", get(live::socket))
        .route("/camera/history/range", get(camera::range))
        .route("/camera/history/frame", get(camera::frame))
        .route("/camera/history/jobs", get(camera::jobs))
        .route("/api/events", get(settings::events))
        .route("/api/printers", get(printers::list).post(printers::add))
        .route(
            "/api/printers/{id}",
            post(printers::update).delete(printers::remove),
        )
        .route("/api/settings", get(settings::get))
        .route("/api/settings/{name}", post(settings::set))
        .route("/api/push/key", get(notify::key))
        .route("/api/push/subscribe", post(notify::subscribe))
        .route("/api/push/unsubscribe", post(notify::unsubscribe))
        .route("/api/push/test", post(notify::test))
        .route(
            "/api/push/preferences",
            get(notify::preferences).post(notify::set_preferences),
        )
        .route("/healthz", get(|| async { StatusCode::OK }))
        .fallback(statics::serve)
        .method_not_allowed_fallback(|| async {
            text(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed")
        })
        .with_state(app)
}

type Query = axum::extract::Query<HashMap<String, String>>;

/// A plain-text reply, the way every error and action result is sent.
pub fn text(status: StatusCode, msg: impl Into<String>) -> Response {
    let mut body = msg.into();
    if !status.is_success() {
        body.push('\n');
    }
    let mut resp = (status, body).into_response();
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

/// A JSON reply.
pub fn json_reply(v: &impl Serialize) -> Response {
    let mut body = serde_json::to_vec(v).unwrap_or_default();
    body.push(b'\n');
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// Decodes a small JSON request body.
fn read_json<T: serde::de::DeserializeOwned>(body: &[u8]) -> Option<T> {
    if body.len() > 4096 {
        return None;
    }
    serde_json::from_slice(body).ok()
}

async fn status(State(app): State<App>, query: Query) -> Response {
    match app.printer(&query) {
        Ok(p) => json_reply(&status_of(p.as_deref(), (app.clock)())),
        Err(refusal) => *refusal,
    }
}

/// What the page shows about a printer, from its merged state and its timers,
/// at `now` (unix milliseconds). With no printer, the same shape with nothing
/// known.
pub fn status_of(printer: Option<&Printer>, now: i64) -> Value {
    let snap = printer.map(|p| p.cache.snapshot()).unwrap_or_default();
    let snap: &Snapshot = &snap;
    let (f, connected) = (&snap.fields, snap.connected);
    let state = p1s::gcode_state(f);
    let get = |name: &str| f.get(name).cloned().unwrap_or(Value::Null);
    let remaining = |name| printer.and_then(|p| p.timers.remaining(name, now));
    let allowed = |action| p1s::print_action_allowed(connected, state, action).is_ok();
    json!({
        "printer": printer.map(|p| p.id),
        "connected": connected,
        "configured": printer.is_some(),
        "problem": snap.problem.as_deref(),
        "gcodeState": state,
        "actionsAllowed": p1s::action_allowed(connected, state).is_ok(),
        "bedTemp": get("bed_temper"),
        "bedTarget": get("bed_target_temper"),
        "nozzleTemp": get("nozzle_temper"),
        "nozzleTarget": get("nozzle_target_temper"),
        "progress": get("mc_percent"),
        "jobName": p1s::job_name(f),
        "layerNum": get("layer_num"),
        "totalLayerNum": get("total_layer_num"),
        "remainingMinutes": get("mc_remaining_time"),
        "chamberLight": p1s::chamber_light(f),
        "fans": {
            "cooling": get("cooling_fan_speed"),
            "aux": get("big_fan1_speed"),
            "chamber": get("big_fan2_speed"),
        },
        "ams": get("ams"),
        "hms": p1s::hms_errors(f),
        "bedOffIn": remaining(timers::BED_OFF),
        "nozzleOffIn": remaining(timers::NOZZLE_OFF),
        "lampOffIn": remaining(timers::LAMP_OFF),
        "printActions": {"pause": allowed("pause"), "resume": allowed("resume"), "stop": allowed("stop")},
    })
}

#[cfg(test)]
pub mod tests;
