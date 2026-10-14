//! GET /api/live: one websocket per open page, carrying everything the page
//! used to poll for.
//!
//! Server to page, as JSON text, each message a full replacement:
//! - `{"type":"printers","printers":[...]}` — every printer's name and how it
//!   is doing, for the switcher, on connect and whenever any of it changes;
//! - `{"type":"status", ...}` — the `/api/status` shape for the selected
//!   printer, on connect, on every change to its state (at most four a
//!   second), and every second so countdowns stay current;
//! - `{"type":"range","oldest","newest"}` — the scrub bar's span, on connect,
//!   per recorded frame and every second;
//! - `{"type":"jobs","jobs":[...]}` — recent prints, on connect and whenever
//!   one starts, ends or is forgotten.
//!
//! Page to server: `{"type":"select","printer":<id>}` picks the printer the
//! status, range, jobs and frames are about (the first one until it does), and
//! `{"type":"follow","on":true}` asks for each recorded frame, sent binary as an
//! 8-byte big-endian unix second then the JPEG.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};
use tokio::time::Instant;

use super::{App, text};
use crate::camera::Frame;
use crate::clock::Clock;
use crate::printers::{Printer, Printers};

/// The fewest milliseconds between two status messages.
const STATUS_EVERY: Duration = Duration::from_millis(250);
const PING_EVERY: Duration = Duration::from_secs(30);

/// The latest of each message about one printer, shared by every page showing
/// it.
#[derive(Clone)]
pub struct Feed {
    status: watch::Sender<Arc<str>>,
    range: watch::Sender<Arc<str>>,
    jobs: watch::Sender<Arc<str>>,
}

impl Default for Feed {
    fn default() -> Self {
        let empty = || watch::Sender::new(Arc::from(""));
        Feed {
            status: empty(),
            range: empty(),
            jobs: empty(),
        }
    }
}

fn publish(tx: &watch::Sender<Arc<str>>, msg: Value) {
    let msg: Arc<str> = msg.to_string().into();
    tx.send_if_modified(|current| {
        let changed = *current != msg;
        if changed {
            *current = msg;
        }
        changed
    });
}

fn tagged(kind: &str, mut v: Value) -> Value {
    if let Value::Object(map) = &mut v {
        map.insert("type".into(), kind.into());
    }
    v
}

impl Feed {
    fn status(&self, printer: &Printer, now: i64) {
        publish(
            &self.status,
            tagged("status", super::status_of(Some(printer), now)),
        );
    }

    fn range(&self, printer: &Printer) {
        match super::camera::range_of(printer) {
            Ok(range) => publish(&self.range, tagged("range", range)),
            Err(err) => tracing::warn!("live: camera range: {err}"),
        }
    }

    fn jobs(&self, printer: &Printer) {
        match printer.store.recent_jobs() {
            Ok(jobs) => publish(&self.jobs, json!({"type": "jobs", "jobs": jobs})),
            Err(err) => tracing::warn!("live: jobs: {err}"),
        }
    }

    /// Keeps one printer's messages current, and the switcher's summary with
    /// them, until the printer is removed.
    pub async fn run(self, printer: Arc<Printer>, printers: Printers, clock: Clock) {
        let mut state = printer.cache.subscribe();
        let mut frames = printer.hub.frames();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut next_status = Instant::now();
        self.jobs(&printer);
        loop {
            let throttled = Instant::now() < next_status;
            tokio::select! {
                changed = state.changed(), if !throttled => {
                    if changed.is_err() {
                        return;
                    }
                    self.status(&printer, clock());
                    printers.refresh_summary();
                    next_status = Instant::now() + STATUS_EVERY;
                }
                _ = tokio::time::sleep_until(next_status), if throttled => {}
                _ = tick.tick() => {
                    self.status(&printer, clock());
                    self.range(&printer);
                }
                frame = frames.recv() => match frame {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => self.range(&printer),
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = printer.jobs_changed.notified() => {
                    self.jobs(&printer);
                    self.range(&printer);
                }
            }
        }
    }

    /// Fills in anything not computed yet, so a page's first messages are
    /// complete.
    fn prime(&self, printer: &Printer, now: i64) {
        self.status(printer, now);
        self.range(printer);
        if self.jobs.borrow().is_empty() {
            self.jobs(printer);
        }
    }
}

pub async fn socket(State(app): State<App>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    // A websocket is opened with GET, which the cross-origin check lets
    // through, and browsers send the session cookie with it from any page. So
    // the origin is checked here: only this app's own page may connect.
    if !same_origin(&app, &headers) {
        return text(StatusCode::FORBIDDEN, "cross-origin websocket refused");
    }
    ws.on_upgrade(move |socket| client(app, socket))
}

fn same_origin(app: &App, headers: &HeaderMap) -> bool {
    let get = |name| {
        headers
            .get(name)
            .and_then(|v: &header::HeaderValue| v.to_str().ok())
    };
    let (Some(origin), Some(host)) = (get(header::ORIGIN), get(header::HOST)) else {
        return false;
    };
    if app.cross_origin.trusted.iter().any(|t| t == origin) {
        return true;
    }
    url::Url::parse(origin).is_ok_and(|u| {
        let origin_host = match u.port() {
            Some(port) => format!("{}:{port}", u.host_str().unwrap_or("")),
            None => u.host_str().unwrap_or("").to_string(),
        };
        origin_host.eq_ignore_ascii_case(host)
    })
}

/// What one page is watching: the selected printer's messages and frames, or
/// a placeholder status when there are no printers.
struct Watching {
    printer: Option<Arc<Printer>>,
    status: watch::Receiver<Arc<str>>,
    range: watch::Receiver<Arc<str>>,
    jobs: watch::Receiver<Arc<str>>,
    frames: broadcast::Receiver<Frame>,
}

impl Watching {
    fn new(printer: Option<Arc<Printer>>, now: i64) -> Watching {
        match &printer {
            Some(p) => {
                p.feed.prime(p, now);
                Watching {
                    status: p.feed.status.subscribe(),
                    range: p.feed.range.subscribe(),
                    jobs: p.feed.jobs.subscribe(),
                    frames: p.hub.frames(),
                    printer,
                }
            }
            None => {
                let fixed = |v: Value| watch::Sender::new(Arc::from(v.to_string())).subscribe();
                Watching {
                    status: fixed(tagged("status", super::status_of(None, now))),
                    range: fixed(json!({"type": "range", "oldest": null, "newest": null})),
                    jobs: fixed(json!({"type": "jobs", "jobs": []})),
                    frames: broadcast::Sender::new(1).subscribe(),
                    printer: None,
                }
            }
        }
    }

    fn id(&self) -> Option<i64> {
        self.printer.as_ref().map(|p| p.id)
    }

    /// Every current message, as on connect or after switching printer.
    fn current(&mut self) -> Vec<Message> {
        [&mut self.status, &mut self.range, &mut self.jobs]
            .into_iter()
            .map(|rx| rx.borrow_and_update().clone())
            .filter(|msg| !msg.is_empty())
            .map(|msg| Message::Text(msg.as_ref().into()))
            .collect()
    }
}

/// What a page asked for.
enum Request {
    Follow(bool),
    Select(i64),
}

fn request(msg: &str) -> Option<Request> {
    let v: Value = serde_json::from_str(msg).ok()?;
    match v.get("type")?.as_str()? {
        "follow" => Some(Request::Follow(v.get("on")?.as_bool()?)),
        "select" => Some(Request::Select(v.get("printer")?.as_i64()?)),
        _ => None,
    }
}

async fn send_all(socket: &mut WebSocket, msgs: Vec<Message>) -> bool {
    for msg in msgs {
        if socket.send(msg).await.is_err() {
            return false;
        }
    }
    true
}

async fn client(app: App, mut socket: WebSocket) {
    let mut summary = app.printers.summary();
    let mut watching = Watching::new(app.printers.first(), (app.clock)());
    let mut following = false;
    let mut ping = tokio::time::interval(PING_EVERY);
    let mut first = vec![Message::Text(summary.borrow_and_update().as_ref().into())];
    first.extend(watching.current());
    if !send_all(&mut socket, first).await {
        return;
    }
    loop {
        let out = tokio::select! {
            Ok(()) = summary.changed() => {
                let msg = Message::Text(summary.borrow_and_update().as_ref().into());
                // A removed printer, or the first one added: move to whichever
                // is first now.
                let gone = watching.id().is_none_or(|id| app.printers.get(id).is_none());
                if gone && watching.id() != app.printers.first().map(|p| p.id) {
                    watching = Watching::new(app.printers.first(), (app.clock)());
                    let mut msgs = vec![msg];
                    msgs.extend(watching.current());
                    if !send_all(&mut socket, msgs).await {
                        return;
                    }
                    continue;
                }
                msg
            }
            Ok(()) = watching.status.changed() => Message::Text(watching.status.borrow_and_update().as_ref().into()),
            Ok(()) = watching.range.changed() => Message::Text(watching.range.borrow_and_update().as_ref().into()),
            Ok(()) = watching.jobs.changed() => Message::Text(watching.jobs.borrow_and_update().as_ref().into()),
            frame = watching.frames.recv(), if following && watching.printer.is_some() => match frame {
                Ok(frame) => binary_frame(frame.0, &frame.1),
                // A slow page skips to the newest frame rather than queueing.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => continue,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(msg))) => match request(&msg) {
                    Some(Request::Select(id)) => {
                        let Some(printer) = app.printers.get(id) else { continue };
                        watching = Watching::new(Some(printer), (app.clock)());
                        let mut msgs = watching.current();
                        if following {
                            msgs.extend(newest_frame(&watching));
                        }
                        if !send_all(&mut socket, msgs).await {
                            return;
                        }
                        continue;
                    }
                    Some(Request::Follow(on)) => {
                        following = on;
                        if !on {
                            continue;
                        }
                        // Show the newest frame at once rather than at the next one.
                        match newest_frame(&watching) {
                            Some(msg) => msg,
                            None => continue,
                        }
                    }
                    None => continue,
                },
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                Some(Ok(_)) => continue,
            },
            _ = ping.tick() => Message::Ping(Vec::new().into()),
        };
        if socket.send(out).await.is_err() {
            return;
        }
    }
}

fn binary_frame(ts: i64, jpeg: &[u8]) -> Message {
    let mut out = Vec::with_capacity(8 + jpeg.len());
    out.extend_from_slice(&ts.to_be_bytes());
    out.extend_from_slice(jpeg);
    Message::Binary(out.into())
}

fn newest_frame(watching: &Watching) -> Option<Message> {
    let store = &watching.printer.as_ref()?.store;
    let (_, newest) = store.range().ok()?;
    let (jpeg, ts) = store.frame_at_or_after(newest?).ok()??;
    Some(binary_frame(ts, &jpeg))
}
