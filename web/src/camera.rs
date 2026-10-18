use dioxus::core::Task;
use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;

use crate::{State, api, http, icons, js, send_live};

const TICK_MS: u32 = 250;
const SPEEDS: [i64; 4] = [30, 60, 300, 600];

#[derive(Clone, Copy)]
pub struct Camera {
    following: Signal<bool>,
    image: Signal<Option<String>>,
    caption: Signal<String>,
    pos: Signal<f64>,
    seq: Signal<u64>,
    playing: Signal<bool>,
    player: Signal<Option<Task>>,
    bound_end: Signal<Option<i64>>,
    job_span: Signal<Option<(i64, i64)>>,
    speed: Signal<i64>,
}

impl Camera {
    pub fn new() -> Camera {
        Camera {
            following: Signal::new(true),
            image: Signal::new(None),
            caption: Signal::new("…".to_string()),
            pos: Signal::new(0.0),
            seq: Signal::new(0),
            playing: Signal::new(false),
            player: Signal::new(None),
            bound_end: Signal::new(None),
            job_span: Signal::new(None),
            speed: Signal::new(60),
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
    camera.pos.set(ts as f64);
    camera.caption.set(js::cam_time(ts as f64, true));
}

pub fn follow_range(state: State, range: api::Range) {
    let mut camera = state.camera;
    if let (true, Some(newest)) = (camera.following(), range.newest) {
        camera.pos.set(newest as f64);
    }
}

fn show_frame(state: State, ts: f64) {
    let mut camera = state.camera;
    let seq = next_seq(camera);
    let url = state.with_printer(&format!("/camera/history/frame?ts={}", ts.floor() as i64));
    spawn(async move {
        let Ok(response) = http::get(&url).await else {
            return;
        };
        if !response.ok() {
            return;
        }
        let taken = response
            .headers()
            .get(api::FRAME_TIMESTAMP)
            .and_then(|t| t.parse::<f64>().ok())
            .unwrap_or(0.0);
        let Ok(jpeg) = response.binary().await else {
            return;
        };
        if *camera.seq.peek() != seq {
            return;
        }
        if let Some(url) = object_url(&jpeg) {
            set_image(camera, url);
        }
        camera
            .caption
            .set(js::cam_time(if taken > 0.0 { taken } else { ts }, true));
    });
}

fn seek(state: State, ts: f64) {
    let mut pos = state.camera.pos;
    pos.set(ts);
    show_frame(state, ts);
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

fn start_playback(state: State) {
    let mut camera = state.camera;
    let Some(cap) = camera.bound_end.peek().or(state.range.peek().newest) else {
        return;
    };
    set_following(camera, false);
    camera.playing.set(true);
    let step = *camera.speed.peek() as f64 * (TICK_MS as f64 / 1000.0);
    let cap = cap as f64;
    let task = spawn(async move {
        loop {
            TimeoutFuture::new(TICK_MS).await;
            let pos = *camera.pos.peek();
            if pos + step >= cap {
                seek(state, cap);
                camera.playing.set(false);
                camera.player.set(None);
                return;
            }
            seek(state, pos + step);
        }
    });
    camera.player.set(Some(task));
}

fn clear_job(mut camera: Camera) {
    stop_playback(camera);
    camera.job_span.set(None);
    camera.bound_end.set(None);
}

pub fn go_live(state: State) {
    clear_job(state.camera);
    set_following(state.camera, true);
}

fn jump_to_live(state: State) {
    go_live(state);
    if let Some(newest) = state.range.peek().newest {
        seek(state, newest as f64);
    }
}

fn scrub(state: State, value: String) {
    set_following(state.camera, false);
    stop_playback(state.camera);
    let ts = value.parse().unwrap_or(0.0);
    seek(state, ts);
}

fn change_speed(state: State, value: String) {
    let mut camera = state.camera;
    camera.speed.set(value.parse().unwrap_or(60));
    if *camera.playing.peek() {
        stop_playback(camera);
        start_playback(state);
    }
}

fn play_job(state: State, job: &api::Job) {
    let mut camera = state.camera;
    let Some(end) = job.end.or(state.range.peek().newest) else {
        return;
    };
    stop_playback(camera);
    set_following(camera, false);
    camera.bound_end.set(Some(end));
    camera.job_span.set(Some((job.start, end)));
    seek(state, job.start as f64);
    start_playback(state);
}

#[component]
pub fn CameraCard() -> Element {
    let state = use_context::<State>();
    let camera = state.camera;
    use_drop(move || stop_playback(camera));
    let range = state.range.cloned();
    let (Some(oldest), Some(newest)) = (range.oldest, range.newest) else {
        return rsx! {
            div { class: "card", id: "camCard",
                div { id: "camEmpty", style: "color:var(--muted)", "No recordings yet" }
            }
        };
    };
    let (min, max) = camera.job_span.cloned().unwrap_or((oldest, newest));
    let following = camera.following.cloned();
    let playing = camera.playing.cloned();
    let pos = camera.pos.cloned().floor() as i64;
    let image = camera.image.cloned();
    let caption = camera.caption.cloned();
    let speed = camera.speed.cloned();
    let jobs = state.jobs.cloned();
    rsx! {
        div { class: "card", id: "camCard",
            div { id: "camControls",
                div { class: "cam-frame",
                    img { id: "camImg", alt: "printer camera", src: image }
                    button {
                        class: if following { "cam-live on" } else { "cam-live" },
                        id: "camLiveBtn",
                        "aria-label": if following { "Live view" } else { "Jump to live view" },
                        onclick: move |_| jump_to_live(state),
                        span { class: "dot" }
                        "LIVE"
                    }
                }
                input {
                    r#type: "range",
                    id: "camSlider",
                    min: "{min}",
                    max: "{max}",
                    step: "1",
                    value: "{pos}",
                    oninput: move |e: FormEvent| scrub(state, e.value()),
                }
                div { class: "row",
                    span { id: "camTime", "{caption}" }
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
                        select {
                            id: "camSpeedSelect",
                            onchange: move |e: FormEvent| change_speed(state, e.value()),
                            for s in SPEEDS {
                                option { key: "{s}", value: "{s}", selected: s == speed, "{s}x" }
                            }
                        }
                    }
                }
                h2 { style: "margin-top:14px", "Recent jobs" }
                div { id: "camJobs",
                    for job in jobs {
                        JobRow { key: "{job.id}", job: job.clone() }
                    }
                }
            }
        }
    }
}

#[component]
fn JobRow(job: api::Job) -> Element {
    let state = use_context::<State>();
    let name = if job.name.is_empty() {
        "(unnamed job)".to_string()
    } else {
        job.name.clone()
    };
    let when = js::cam_time(job.start as f64, false);
    let label = if job.end.is_some() {
        "Timelapse"
    } else {
        "Watch job so far"
    };
    rsx! {
        div { class: "row",
            span {
                "{name}"
                span { class: "cam-job-time", "{when}" }
            }
            button {
                style: "padding:6px 14px; font-size:0.8rem; border-radius:8px",
                onclick: move |_| play_job(state, &job),
                "{label}"
            }
        }
    }
}
