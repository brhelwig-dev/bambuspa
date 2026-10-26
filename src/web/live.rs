use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use bambuspa_api::{ClientMessage, ServerMessage};
use tokio::sync::{broadcast, watch};
use tokio::time::Instant;

use super::{App, text};
use crate::camera::Frame;
use crate::clock::Clock;
use crate::printers::{Printer, Printers};

const STATUS_EVERY: Duration = Duration::from_millis(250);
const PING_EVERY: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct Feed {
    status: watch::Sender<Arc<str>>,
    timelapses: watch::Sender<Arc<str>>,
}

impl Default for Feed {
    fn default() -> Self {
        let empty = || watch::Sender::new(Arc::from(""));
        Feed {
            status: empty(),
            timelapses: empty(),
        }
    }
}

fn encode(msg: &ServerMessage) -> Arc<str> {
    serde_json::to_string(msg).unwrap_or_default().into()
}

fn publish(tx: &watch::Sender<Arc<str>>, msg: &ServerMessage) {
    let msg = encode(msg);
    tx.send_if_modified(|current| {
        let changed = *current != msg;
        if changed {
            *current = msg;
        }
        changed
    });
}

impl Feed {
    fn status(&self, printer: &Printer, now: i64) {
        publish(
            &self.status,
            &ServerMessage::Status(Box::new(super::status_of(Some(printer), now))),
        );
    }

    fn timelapses(&self, printer: &Printer) {
        let timelapses = printer.library.list().borrow().clone();
        publish(&self.timelapses, &ServerMessage::Timelapses { timelapses });
    }

    pub async fn run(self, printer: Arc<Printer>, printers: Printers, clock: Clock) {
        let mut state = printer.cache.subscribe();
        let mut timelapses = printer.library.list();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut next_status = Instant::now();
        self.timelapses(&printer);
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
                _ = tick.tick() => self.status(&printer, clock()),
                changed = timelapses.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    self.timelapses(&printer);
                }
            }
        }
    }

    fn prime(&self, printer: &Printer, now: i64) {
        self.status(printer, now);
        if self.timelapses.borrow().is_empty() {
            self.timelapses(printer);
        }
    }
}

pub async fn socket(State(app): State<App>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
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

struct Watching {
    printer: Option<Arc<Printer>>,
    status: watch::Receiver<Arc<str>>,
    timelapses: watch::Receiver<Arc<str>>,
    frames: broadcast::Receiver<Frame>,
}

impl Watching {
    fn new(printer: Option<Arc<Printer>>, now: i64) -> Watching {
        match &printer {
            Some(p) => {
                p.feed.prime(p, now);
                Watching {
                    status: p.feed.status.subscribe(),
                    timelapses: p.feed.timelapses.subscribe(),
                    frames: p.hub.frames(),
                    printer,
                }
            }
            None => {
                let fixed = |msg: ServerMessage| watch::Sender::new(encode(&msg)).subscribe();
                Watching {
                    status: fixed(ServerMessage::Status(Box::new(super::status_of(None, now)))),
                    timelapses: fixed(ServerMessage::Timelapses {
                        timelapses: Vec::new(),
                    }),
                    frames: broadcast::Sender::new(1).subscribe(),
                    printer: None,
                }
            }
        }
    }

    fn id(&self) -> Option<i64> {
        self.printer.as_ref().map(|p| p.id)
    }

    fn current(&mut self) -> Vec<Message> {
        [&mut self.status, &mut self.timelapses]
            .into_iter()
            .map(|rx| rx.borrow_and_update().clone())
            .filter(|msg| !msg.is_empty())
            .map(|msg| Message::Text(msg.as_ref().into()))
            .collect()
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
            Ok(()) = watching.timelapses.changed() => Message::Text(watching.timelapses.borrow_and_update().as_ref().into()),
            frame = watching.frames.recv(), if following && watching.printer.is_some() => match frame {
                Ok(frame) => binary_frame(frame.0, &frame.1),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => continue,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(msg))) => match serde_json::from_str(&msg).ok() {
                    Some(ClientMessage::Select { printer: id }) => {
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
                    Some(ClientMessage::Follow { on }) => {
                        following = on;
                        if !on {
                            continue;
                        }
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
    let frame = watching.printer.as_ref()?.hub.latest()?;
    Some(binary_frame(frame.0, &frame.1))
}
