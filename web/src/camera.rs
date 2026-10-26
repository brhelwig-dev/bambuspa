use dioxus::core::Task;
use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use wasm_bindgen::JsCast;

use crate::{State, api, http, icons, js};

const PLAYER_MARGIN: i32 = 10;

#[derive(Clone, PartialEq)]
struct Video {
    frames: usize,
    interval_ms: u32,
}

#[derive(Clone, Copy)]
pub struct Camera {
    image: Signal<Option<String>>,
    caption: Signal<String>,
    open: Signal<Option<String>>,
    video: Signal<Option<Video>>,
    frame: Signal<Option<String>>,
    status: Signal<String>,
    seq: Signal<u64>,
    pos: Signal<usize>,
    playing: Signal<bool>,
    player: Signal<Option<Task>>,
}

impl Camera {
    pub fn new() -> Camera {
        Camera {
            image: Signal::new(None),
            caption: Signal::new(String::new()),
            open: Signal::new(None),
            video: Signal::new(None),
            frame: Signal::new(None),
            status: Signal::new(String::new()),
            seq: Signal::new(0),
            pos: Signal::new(0),
            playing: Signal::new(false),
            player: Signal::new(None),
        }
    }
}

fn object_url(jpeg: &[u8]) -> Option<String> {
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(jpeg));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type("image/jpeg");
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options).ok()?;
    web_sys::Url::create_object_url_with_blob(&blob).ok()
}

fn replace_url(mut slot: Signal<Option<String>>, url: Option<String>) {
    let old = slot.peek().clone();
    slot.set(url);
    if let Some(old) = old {
        let _ = web_sys::Url::revoke_object_url(&old);
    }
}

pub fn show_live_frame(state: State, ts: i64, jpeg: &[u8]) {
    let mut camera = state.camera;
    if let Some(url) = object_url(jpeg) {
        replace_url(camera.image, Some(url));
    }
    camera.caption.set(js::cam_time(ts as f64, true));
}

fn recorded(name: &str) -> String {
    let stem = name.trim_start_matches("video_").trim_end_matches(".avi");
    match stem.split_once('_') {
        Some((day, time)) => format!("{day} {}", time.replace('-', ":").get(..5).unwrap_or("")),
        None => stem.to_string(),
    }
}

fn timelapse_url(state: State, what: &str, name: &str) -> String {
    state.with_printer(&format!("/api/timelapses/{what}?name={}", js::encode(name)))
}

async fn show_frame(state: State, name: &str, i: usize) -> Option<(usize, u32)> {
    let mut camera = state.camera;
    let seq = {
        let mut seq = camera.seq.write();
        *seq += 1;
        *seq
    };
    let current = move || *camera.seq.peek() == seq;
    let response = http::get(&format!("{}&i={i}", timelapse_url(state, "frame", name)))
        .await
        .ok()?;
    if !response.ok() {
        let reason = response.text().await.unwrap_or_default();
        if current() {
            camera
                .status
                .set(format!("Couldn't play it: {}", reason.trim()));
        }
        return None;
    }
    let header = |h: &str| {
        response
            .headers()
            .get(h)
            .and_then(|v| v.parse::<u32>().ok())
    };
    let frames = header(api::FRAME_COUNT)? as usize;
    let interval_us = header(api::FRAME_INTERVAL)?;
    let jpeg = response.binary().await.ok()?;
    if !current() {
        return None;
    }
    if let Some(url) = object_url(&jpeg) {
        replace_url(camera.frame, Some(url));
    }
    camera.status.set(format!("{} of {frames}", i + 1));
    Some((frames, interval_us / 1000))
}

fn stop_playback(mut camera: Camera) {
    camera.playing.set(false);
    let player = *camera.player.peek();
    camera.player.set(None);
    if let Some(task) = player {
        task.cancel();
    }
}

fn start_playback(state: State) {
    let mut camera = state.camera;
    let (Some(name), Some(video)) = (camera.open.peek().clone(), camera.video.peek().clone())
    else {
        return;
    };
    camera.playing.set(true);
    let task = spawn(async move {
        loop {
            let mut next = *camera.pos.peek() + 1;
            if next >= video.frames {
                next = 0;
            }
            let started = js_sys::Date::now();
            if show_frame(state, &name, next).await.is_none() {
                break;
            }
            camera.pos.set(next);
            if next + 1 >= video.frames {
                break;
            }
            let spent = (js_sys::Date::now() - started) as u32;
            TimeoutFuture::new(video.interval_ms.saturating_sub(spent)).await;
        }
        camera.playing.set(false);
        camera.player.set(None);
    });
    camera.player.set(Some(task));
}

pub fn close_player(state: State) {
    let mut camera = state.camera;
    stop_playback(camera);
    *camera.seq.write() += 1;
    camera.open.set(None);
    camera.video.set(None);
    camera.status.set(String::new());
    replace_url(camera.frame, None);
}

fn open(state: State, name: String) {
    let mut camera = state.camera;
    close_player(state);
    camera.open.set(Some(name.clone()));
    camera.pos.set(0);
    camera.status.set("Loading…".to_string());
    spawn(async move {
        let Some((frames, interval_ms)) = show_frame(state, &name, 0).await else {
            return;
        };
        camera.video.set(Some(Video {
            frames,
            interval_ms,
        }));
        start_playback(state);
    });
}

fn scrub(state: State, value: String) {
    let mut camera = state.camera;
    stop_playback(camera);
    let (Some(name), Some(video)) = (camera.open.peek().clone(), camera.video.peek().clone())
    else {
        return;
    };
    let i = value.parse::<usize>().unwrap_or(0).min(video.frames - 1);
    camera.pos.set(i);
    spawn(async move {
        show_frame(state, &name, i).await;
    });
}

fn timelapse_list() -> Option<web_sys::Element> {
    js::window().document()?.get_element_by_id("camTimelapses")
}

fn make_room_for_player() {
    let Some(list) = timelapse_list() else {
        return;
    };
    let player = list
        .query_selector(".tl-player")
        .ok()
        .flatten()
        .and_then(|p| p.dyn_into::<web_sys::HtmlElement>().ok());
    let Some(player) = player else {
        return;
    };
    let row = player
        .previous_element_sibling()
        .and_then(|r| r.dyn_into::<web_sys::HtmlElement>().ok());
    let _ = list.set_attribute(
        "style",
        &format!(
            "max-height: calc(5 * var(--tl-row) + {}px)",
            player.offset_height() + PLAYER_MARGIN
        ),
    );
    if let Some(row) = row {
        list.set_scroll_top(row.offset_top());
    }
}

fn shrink_list() {
    if let Some(list) = timelapse_list() {
        let _ = list.remove_attribute("style");
    }
}

#[component]
pub fn CameraCard() -> Element {
    let state = use_context::<State>();
    use_drop(move || close_player(state));
    let image = state.camera.image.cloned();
    let caption = state.camera.caption.cloned();
    let show_caption = image.is_some() && !caption.is_empty();
    let timelapses = state.timelapses.cloned();
    rsx! {
        div { class: "card", id: "camCard",
            div { class: "cam-frame",
                if let Some(image) = image {
                    img { id: "camImg", alt: "printer camera", src: image }
                    span { class: "cam-live on", id: "camLive",
                        span { class: "dot" }
                        "LIVE"
                    }
                }
            }
            if show_caption {
                div { id: "camTime", "{caption}" }
            }
            h2 { style: "margin-top:14px", "Timelapses on the SD card" }
            div { id: "camTimelapses",
                if timelapses.is_empty() {
                    div { style: "color:var(--muted)",
                        "None yet. Turn on timelapse in Bambu Studio when you send a print."
                    }
                }
                for timelapse in timelapses {
                    TimelapseRow { key: "{timelapse.name}", timelapse: timelapse.clone() }
                }
            }
        }
    }
}

#[component]
fn TimelapseRow(timelapse: api::Timelapse) -> Element {
    let state = use_context::<State>();
    let thumbnail = timelapse_url(state, "thumbnail", &timelapse.name);
    let size = format!("{:.1} MB", timelapse.bytes as f64 / (1 << 20) as f64);
    let is_open = state.camera.open.read().as_deref() == Some(timelapse.name.as_str());
    let name = timelapse.name.clone();
    rsx! {
        div { class: "row tl-row",
            img { class: "tl-thumb", alt: "", loading: "lazy", src: "{thumbnail}" }
            span { class: "tl-name",
                "{recorded(&timelapse.name)}"
                span { class: "cam-job-time", "{size}" }
            }
            button {
                class: if is_open { "tl-btn stop" } else { "tl-btn" },
                onclick: move |_| {
                    if is_open {
                        close_player(state);
                    } else {
                        open(state, name.clone());
                    }
                },
                if is_open { "Stop" } else { "Play" }
            }
        }
        if is_open {
            TimelapsePlayer {}
        }
    }
}

#[component]
fn TimelapsePlayer() -> Element {
    let state = use_context::<State>();
    let camera = state.camera;
    use_effect(make_room_for_player);
    use_drop(shrink_list);
    let frame = camera.frame.cloned();
    let video = camera.video.cloned();
    let playing = camera.playing.cloned();
    let pos = camera.pos.cloned();
    let status = camera.status.cloned();
    rsx! {
        div { class: "tl-player", id: "tlPlayer",
            div { class: "cam-frame",
                if let Some(frame) = frame {
                    img { alt: "timelapse frame", src: frame }
                }
            }
            div { class: "tl-controls",
                button {
                    class: "camctl",
                    id: "camPlayBtn",
                    disabled: video.is_none(),
                    "aria-label": if playing { "Pause timelapse" } else { "Play timelapse" },
                    dangerous_inner_html: if playing { icons::PAUSE } else { icons::PLAY },
                    onclick: move |_| {
                        if *camera.playing.peek() {
                            stop_playback(camera);
                        } else {
                            start_playback(state);
                        }
                    },
                }
                input {
                    r#type: "range",
                    id: "camSlider",
                    "aria-label": "Timelapse position",
                    min: "0",
                    max: "{video.as_ref().map_or(0, |v| v.frames - 1)}",
                    step: "1",
                    value: "{pos}",
                    disabled: video.is_none(),
                    oninput: move |e: FormEvent| scrub(state, e.value()),
                }
                span { class: "tl-count", "{status}" }
            }
        }
    }
}
