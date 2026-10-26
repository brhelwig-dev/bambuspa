use dioxus::core::Task;
use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;

use crate::{State, api, http, icons, js, send_live};

#[derive(Clone, PartialEq)]
struct Video {
    name: String,
    frames: usize,
    interval_ms: u32,
}

#[derive(Clone, Copy)]
pub struct Camera {
    following: Signal<bool>,
    image: Signal<Option<String>>,
    caption: Signal<String>,
    seq: Signal<u64>,
    video: Signal<Option<Video>>,
    pos: Signal<usize>,
    playing: Signal<bool>,
    player: Signal<Option<Task>>,
}

impl Camera {
    pub fn new() -> Camera {
        Camera {
            following: Signal::new(true),
            image: Signal::new(None),
            caption: Signal::new("…".to_string()),
            seq: Signal::new(0),
            video: Signal::new(None),
            pos: Signal::new(0),
            playing: Signal::new(false),
            player: Signal::new(None),
        }
    }

    pub fn following(&self) -> bool {
        *self.following.peek()
    }
}

fn object_url(jpeg: &[u8]) -> Option<String> {
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(jpeg));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type("image/jpeg");
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options).ok()?;
    web_sys::Url::create_object_url_with_blob(&blob).ok()
}

fn set_image(mut camera: Camera, url: String) {
    let old = camera.image.peek().clone();
    camera.image.set(Some(url));
    if let Some(old) = old {
        let _ = web_sys::Url::revoke_object_url(&old);
    }
}

fn next_seq(mut camera: Camera) -> u64 {
    let mut seq = camera.seq.write();
    *seq += 1;
    *seq
}

pub fn show_live_frame(state: State, ts: i64, jpeg: &[u8]) {
    let mut camera = state.camera;
    next_seq(camera);
    if let Some(url) = object_url(jpeg) {
        set_image(camera, url);
    }
    camera.caption.set(js::cam_time(ts as f64, true));
}

/// "video_2026-07-23_08-45-39.avi" as "2026-07-23 08:45".
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

/// Fetches and shows frame `i` of `name`. On success, returns the video's
/// frame count and frame interval, which every frame's response carries.
async fn show_frame(state: State, name: &str, i: usize) -> Option<(usize, u32)> {
    let mut camera = state.camera;
    let seq = next_seq(camera);
    let response = http::get(&format!("{}&i={i}", timelapse_url(state, "frame", name)))
        .await
        .ok()?;
    if !response.ok() {
        let reason = response.text().await.unwrap_or_default();
        if *camera.seq.peek() == seq {
            camera
                .caption
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
    if *camera.seq.peek() != seq {
        return None;
    }
    if let Some(url) = object_url(&jpeg) {
        set_image(camera, url);
    }
    camera
        .caption
        .set(format!("{} · {} of {frames}", recorded(name), i + 1));
    Some((frames, interval_us / 1000))
}

fn set_following(mut camera: Camera, on: bool) {
    camera.following.set(on);
    send_live(api::ClientMessage::Follow { on });
}

fn stop_playback(mut camera: Camera) {
    camera.playing.set(false);
    let player = *camera.player.peek();
    camera.player.set(None);
    if let Some(task) = player {
        task.cancel();
    }
}

/// Plays from the current frame, fetching each one in turn and waiting out the
/// rest of its interval, so a slow connection plays slower rather than skipping.
fn start_playback(state: State) {
    let mut camera = state.camera;
    let Some(video) = camera.video.peek().clone() else {
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
            if show_frame(state, &video.name, next).await.is_none() {
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

fn open(state: State, name: String) {
    let mut camera = state.camera;
    stop_playback(camera);
    set_following(camera, false);
    camera.video.set(None);
    camera.pos.set(0);
    camera.caption.set(format!("{} · loading", recorded(&name)));
    spawn(async move {
        let Some((frames, interval_ms)) = show_frame(state, &name, 0).await else {
            return;
        };
        camera.video.set(Some(Video {
            name,
            frames,
            interval_ms,
        }));
        start_playback(state);
    });
}

pub fn go_live(state: State) {
    let mut camera = state.camera;
    stop_playback(camera);
    camera.video.set(None);
    set_following(camera, true);
}

fn scrub(state: State, value: String) {
    let mut camera = state.camera;
    stop_playback(camera);
    let Some(video) = camera.video.peek().clone() else {
        return;
    };
    let i = value.parse::<usize>().unwrap_or(0).min(video.frames - 1);
    camera.pos.set(i);
    spawn(async move {
        show_frame(state, &video.name, i).await;
    });
}

#[component]
pub fn CameraCard() -> Element {
    let state = use_context::<State>();
    let camera = state.camera;
    use_drop(move || stop_playback(camera));
    let following = camera.following.cloned();
    let playing = camera.playing.cloned();
    let image = camera.image.cloned();
    let caption = camera.caption.cloned();
    let video = camera.video.cloned();
    let has_video = video.is_some();
    let pos = camera.pos.cloned();
    let timelapses = state.timelapses.cloned();
    rsx! {
        div { class: "card", id: "camCard",
            div { id: "camControls",
                div { class: "cam-frame",
                    if image.is_some() {
                        img { id: "camImg", alt: "printer camera", src: image }
                    } else {
                        div { id: "camEmpty", style: "color:var(--muted)", "Waiting for the camera" }
                    }
                    button {
                        class: if following { "cam-live on" } else { "cam-live" },
                        id: "camLiveBtn",
                        "aria-label": if following { "Live view" } else { "Back to the live view" },
                        onclick: move |_| go_live(state),
                        span { class: "dot" }
                        "LIVE"
                    }
                }
                if let Some(video) = video {
                    input {
                        r#type: "range",
                        id: "camSlider",
                        min: "0",
                        max: "{video.frames - 1}",
                        step: "1",
                        value: "{pos}",
                        oninput: move |e: FormEvent| scrub(state, e.value()),
                    }
                }
                div { class: "row",
                    span { id: "camTime", "{caption}" }
                    if has_video {
                        span { class: "camctls",
                            button {
                                class: "camctl",
                                id: "camPlayBtn",
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
                        }
                    }
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
}

#[component]
fn TimelapseRow(timelapse: api::Timelapse) -> Element {
    let state = use_context::<State>();
    let thumbnail = timelapse_url(state, "thumbnail", &timelapse.name);
    let size = format!("{:.1} MB", timelapse.bytes as f64 / (1 << 20) as f64);
    let name = timelapse.name.clone();
    rsx! {
        div { class: "row tl-row",
            img { class: "tl-thumb", alt: "", loading: "lazy", src: "{thumbnail}" }
            span { class: "tl-name",
                "{recorded(&timelapse.name)}"
                span { class: "cam-job-time", "{size}" }
            }
            button {
                style: "padding:6px 14px; font-size:0.8rem; border-radius:8px",
                onclick: move |_| open(state, name.clone()),
                "Play"
            }
        }
    }
}
