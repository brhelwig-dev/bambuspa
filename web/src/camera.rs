use dioxus::prelude::*;

use crate::player::{megabytes, open_window, recorded, show_jpeg, timelapse_url};
use crate::{State, api, icons, js};

const RECENT: usize = 3;

#[derive(Clone, Copy)]
pub struct Camera {
    image: Signal<Option<String>>,
    caption: Signal<String>,
}

impl Camera {
    pub fn new() -> Camera {
        Camera {
            image: Signal::new(None),
            caption: Signal::new(String::new()),
        }
    }
}

pub fn show_live_frame(state: State, ts: i64, jpeg: &[u8]) {
    let mut camera = state.camera;
    show_jpeg(camera.image, jpeg);
    camera.caption.set(js::cam_time(ts as f64, true));
}

#[component]
pub fn CameraCard() -> Element {
    let state = use_context::<State>();
    let mut all = use_signal(|| false);
    let image = state.camera.image.cloned();
    let caption = state.camera.caption.cloned();
    let show_caption = image.is_some() && !caption.is_empty();
    let timelapses = state.timelapses.cloned();
    let shown = if all() { timelapses.len() } else { RECENT };
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
            h2 { style: "margin-top:14px", "Timelapses" }
            div { id: "camTimelapses", class: if all() { "all" } else { "" },
                if timelapses.is_empty() {
                    div { class: "tl-none",
                        "None yet. Turn on timelapse in Bambu Studio when you send a print."
                    }
                }
                for timelapse in timelapses.iter().take(shown) {
                    TimelapseRow { key: "{timelapse.name}", timelapse: timelapse.clone() }
                }
            }
            if timelapses.len() > RECENT {
                button { class: "tl-more", onclick: move |_| all.toggle(),
                    if all() {
                        "Show fewer"
                    } else {
                        "Show all {timelapses.len()}"
                    }
                }
            }
        }
    }
}

#[component]
fn TimelapseRow(timelapse: api::Timelapse) -> Element {
    let state = use_context::<State>();
    let mut no_thumbnail = use_signal(|| false);
    let thumbnail = timelapse_url(state, "thumbnail", &timelapse.name);
    let name = timelapse.name.clone();
    rsx! {
        button {
            class: "tl-row",
            title: "Play in a new window",
            onclick: move |_| open_window(state, &name),
            if no_thumbnail() {
                span { class: "tl-thumb none", dangerous_inner_html: icons::FILM }
            } else {
                img {
                    class: "tl-thumb",
                    alt: "",
                    loading: "lazy",
                    src: "{thumbnail}",
                    onerror: move |_| no_thumbnail.set(true),
                }
            }
            span { class: "tl-name",
                "{recorded(&timelapse.name)}"
                span { class: "tl-size", "{megabytes(timelapse.bytes)}" }
            }
            span { class: "tl-play", dangerous_inner_html: icons::PLAY }
        }
    }
}
