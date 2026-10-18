use dioxus::prelude::*;
use futures_util::future::{Either, select};
use futures_util::{SinkExt, StreamExt};
use gloo_net::websocket::Message;
use gloo_net::websocket::futures::WebSocket;
use gloo_timers::future::TimeoutFuture;

use crate::{State, api, apply_printer_list, camera, http, js};

fn url() -> String {
    let location = js::window().location();
    let scheme = match location.protocol().as_deref() {
        Ok("https:") => "wss://",
        _ => "ws://",
    };
    format!("{scheme}{}/api/live", location.host().unwrap_or_default())
}

fn encode(msg: &api::ClientMessage) -> Message {
    Message::Text(serde_json::to_string(msg).unwrap_or_default())
}

fn receive(mut state: State, msg: Message) {
    match msg {
        Message::Bytes(frame) if frame.len() >= 8 => {
            if state.camera.following() {
                let ts = i64::from_be_bytes(frame[..8].try_into().unwrap_or_default());
                camera::show_live_frame(state, ts, &frame[8..]);
            }
        }
        Message::Bytes(_) => {}
        Message::Text(text) => match serde_json::from_str(&text) {
            Ok(api::ServerMessage::Printers { printers }) => apply_printer_list(state, printers),
            Ok(api::ServerMessage::Status(status)) => {
                let current = *state.current.peek();
                let known = state.printers.peek().iter().any(|p| Some(p.id) == current);
                if status.printer.is_some() && status.printer != current && !known {
                    state.current.set(status.printer);
                }
                state.status.set(Some(*status));
                state.reachable.set(true);
            }
            Ok(api::ServerMessage::Range(range)) => {
                camera::follow_range(state, range);
                state.range.set(range);
            }
            Ok(api::ServerMessage::Jobs { jobs }) => state.jobs.set(jobs),
            Err(_) => {}
        },
    }
}

pub async fn run(mut outbox: UnboundedReceiver<api::ClientMessage>, mut state: State) {
    let mut retry = 1000;
    loop {
        while outbox.try_recv().is_ok() {}
        let mut opened = false;
        if let Ok(socket) = WebSocket::open(&url()) {
            let (mut sink, mut stream) = socket.split();
            let mut hello = Vec::new();
            if let Some(printer) = *state.current.peek() {
                hello.push(api::ClientMessage::Select { printer });
            }
            hello.push(api::ClientMessage::Follow {
                on: state.camera.following(),
            });
            let mut sent = true;
            for msg in &hello {
                sent = sent && sink.send(encode(msg)).await.is_ok();
            }
            while sent {
                match select(stream.next(), outbox.next()).await {
                    Either::Left((Some(Ok(msg)), _)) => {
                        if !opened {
                            opened = true;
                            retry = 1000;
                        }
                        receive(state, msg);
                    }
                    Either::Left(_) => break,
                    Either::Right((Some(msg), _)) => {
                        sent = sink.send(encode(&msg)).await.is_ok();
                    }
                    Either::Right((None, _)) => return,
                }
            }
        }
        state.reachable.set(false);
        if !opened {
            let _ = http::get("/api/status").await;
        }
        TimeoutFuture::new(retry).await;
        retry = (retry * 2).min(30_000);
    }
}
