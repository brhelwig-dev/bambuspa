use dioxus::core::Task;
use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;

use crate::{State, api, http, icons, js};

const POLL_MS: u32 = 300;
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// When a timelapse was recorded, from its file name: "Aug 5, 10:30", with the
/// year added when it isn't this one.
pub fn recorded(name: &str) -> String {
    let stem = name.trim_start_matches("video_").trim_end_matches(".avi");
    let Some((day, time)) = stem.split_once('_') else {
        return stem.to_string();
    };
    let time = time.replace('-', ":");
    let time = time.get(..5).unwrap_or("");
    let mut parts = day.splitn(3, '-').map(|p| p.parse::<u32>().ok());
    let (Some(Some(year)), Some(Some(month)), Some(Some(date))) =
        (parts.next(), parts.next(), parts.next())
    else {
        return format!("{day} {time}");
    };
    let Some(month) = MONTHS.get(month.wrapping_sub(1) as usize) else {
        return format!("{day} {time}");
    };
    if year == js_sys::Date::new_0().get_full_year() {
        format!("{month} {date}, {time}")
    } else {
        format!("{month} {date} {year}, {time}")
    }
}

pub fn megabytes(bytes: i64) -> String {
    format!("{:.1} MB", bytes as f64 / (1 << 20) as f64)
}

pub fn timelapse_url(state: State, what: &str, name: &str) -> String {
    state.with_printer(&format!("/api/timelapses/{what}?name={}", js::encode(name)))
}

/// Opens a timelapse in a window of its own.
pub fn open_window(state: State, name: &str) {
    let url = state.with_printer(&format!("/?timelapse={}", js::encode(name)));
    let opened = js::window().open_with_url_and_target_and_features(
        &url,
        "_blank",
        "popup,width=960,height=620",
    );
    if !matches!(opened, Ok(Some(_))) {
        js::go_to(&url);
    }
}

#[derive(Clone, PartialEq)]
struct Video {
    frames: usize,
    interval_ms: u32,
}

#[derive(Clone, Copy)]
struct Player {
    name: Signal<String>,
    download: Signal<Option<api::TimelapseDownload>>,
    problem: Signal<Option<String>>,
    video: Signal<Option<Video>>,
    frame: Signal<Option<String>>,
    seq: Signal<u64>,
    pos: Signal<usize>,
    playing: Signal<bool>,
    task: Signal<Option<Task>>,
}

fn object_url(jpeg: &[u8]) -> Option<String> {
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(jpeg));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type("image/jpeg");
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options).ok()?;
    web_sys::Url::create_object_url_with_blob(&blob).ok()
}

pub fn replace_url(mut slot: Signal<Option<String>>, url: Option<String>) {
    let old = slot.peek().clone();
    slot.set(url);
    if let Some(old) = old {
        let _ = web_sys::Url::revoke_object_url(&old);
    }
}

pub fn show_jpeg(slot: Signal<Option<String>>, jpeg: &[u8]) {
    if let Some(url) = object_url(jpeg) {
        replace_url(slot, Some(url));
    }
}

/// Has the server copy the timelapse off the SD card, following its progress
/// until every frame can be had at once.
async fn download(state: State, mut player: Player) -> Result<(), String> {
    let url = timelapse_url(state, "download", &player.name.peek());
    let mut response = http::post(&url).await;
    loop {
        let reply = response?;
        if !reply.ok() {
            return Err(http::reply_text(reply).await);
        }
        let progress = reply
            .json::<api::TimelapseDownload>()
            .await
            .map_err(|e| e.to_string())?;
        if let Some(error) = progress.error {
            return Err(error);
        }
        let ready = progress.ready;
        player.download.set(Some(progress));
        if ready {
            return Ok(());
        }
        TimeoutFuture::new(POLL_MS).await;
        response = http::get(&url).await;
    }
}

async fn show_frame(state: State, mut player: Player, i: usize) -> Option<Video> {
    let seq = {
        let mut seq = player.seq.write();
        *seq += 1;
        *seq
    };
    let current = move || *player.seq.peek() == seq;
    let url = format!(
        "{}&i={i}",
        timelapse_url(state, "frame", &player.name.peek())
    );
    let response = http::get(&url).await.ok()?;
    if !response.ok() {
        let reason = http::reply_text(response).await;
        if current() {
            player
                .problem
                .set(Some(format!("Couldn't play it: {reason}")));
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
    show_jpeg(player.frame, &jpeg);
    Some(Video {
        frames,
        interval_ms: interval_us / 1000,
    })
}

fn stop_playback(mut player: Player) {
    player.playing.set(false);
    let task = *player.task.peek();
    player.task.set(None);
    if let Some(task) = task {
        task.cancel();
    }
}

fn start_playback(state: State, mut player: Player) {
    let Some(video) = player.video.peek().clone() else {
        return;
    };
    player.playing.set(true);
    let task = spawn(async move {
        loop {
            let mut next = *player.pos.peek() + 1;
            if next >= video.frames {
                next = 0;
            }
            let started = js_sys::Date::now();
            if show_frame(state, player, next).await.is_none() {
                break;
            }
            player.pos.set(next);
            if next + 1 >= video.frames {
                break;
            }
            let spent = (js_sys::Date::now() - started) as u32;
            TimeoutFuture::new(video.interval_ms.saturating_sub(spent)).await;
        }
        player.playing.set(false);
        player.task.set(None);
    });
    player.task.set(Some(task));
}

fn scrub(state: State, mut player: Player, value: String) {
    stop_playback(player);
    let Some(video) = player.video.peek().clone() else {
        return;
    };
    let i = value.parse::<usize>().unwrap_or(0).min(video.frames - 1);
    player.pos.set(i);
    spawn(async move {
        show_frame(state, player, i).await;
    });
}

async fn load(state: State, mut player: Player) {
    if let Err(problem) = download(state, player).await {
        player
            .problem
            .set(Some(format!("Couldn't copy it off the printer: {problem}")));
        return;
    }
    let Some(video) = show_frame(state, player, 0).await else {
        return;
    };
    player.video.set(Some(video));
    start_playback(state, player);
}

/// The whole page of a window opened to play one timelapse.
#[component]
pub fn PlayerWindow() -> Element {
    let state = use_context_provider(State::new);
    let player = use_hook(|| {
        let name = js::query_param("timelapse")
            .map(|n| js::decode(&n))
            .unwrap_or_default();
        Player {
            name: Signal::new(name),
            download: Signal::new(None),
            problem: Signal::new(None),
            video: Signal::new(None),
            frame: Signal::new(None),
            seq: Signal::new(0),
            pos: Signal::new(0),
            playing: Signal::new(false),
            task: Signal::new(None),
        }
    });
    let title = recorded(&player.name.read());
    use_hook(|| {
        if let Some(document) = js::window().document() {
            document.set_title(&format!("Timelapse · {title}"));
        }
        spawn(load(state, player));
    });
    let frame = player.frame.cloned();
    let video = player.video.cloned();
    let playing = player.playing.cloned();
    let pos = player.pos.cloned();
    let problem = player.problem.cloned();
    let download = player.download.cloned().unwrap_or_default();
    let percent = if download.total > 0 {
        (download.bytes as f64 * 100.0 / download.total as f64).clamp(0.0, 100.0)
    } else {
        0.0
    };
    let late_problem = problem.clone().filter(|_| frame.is_some());
    let count = match &video {
        Some(v) => format!("{} of {}", pos + 1, v.frames),
        None => String::new(),
    };
    rsx! {
        div { class: "tlwin",
            div { class: "topbar",
                span { class: "screentitle", "{title}" }
                if download.total > 0 {
                    span { class: "tl-size", "{megabytes(download.total)}" }
                }
            }
            div { class: "tl-player",
                div { class: "cam-frame",
                    if let Some(frame) = &frame {
                        img { alt: "timelapse frame", src: "{frame}" }
                    } else if let Some(problem) = &problem {
                        div { class: "tl-overlay bad", "{problem}" }
                    } else {
                        div { class: "tl-overlay",
                            div { class: "tl-loading",
                                div { class: "tl-loading-what",
                                    if download.ready { "Opening…" } else { "Copying from the printer…" }
                                }
                                div {
                                    class: "tl-progress",
                                    role: "progressbar",
                                    "aria-valuemin": "0",
                                    "aria-valuemax": "100",
                                    "aria-valuenow": "{percent:.0}",
                                    div { class: "bar", style: "width: {percent:.1}%" }
                                }
                                div { class: "tl-loading-count",
                                    "{megabytes(download.bytes)} of {megabytes(download.total)}"
                                }
                            }
                        }
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
                            if *player.playing.peek() {
                                stop_playback(player);
                            } else {
                                start_playback(state, player);
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
                        oninput: move |e: FormEvent| scrub(state, player, e.value()),
                    }
                    span { class: "tl-count", "{count}" }
                }
                if let Some(problem) = late_problem {
                    div { class: "tl-problem bad", "{problem}" }
                }
            }
        }
    }
}
