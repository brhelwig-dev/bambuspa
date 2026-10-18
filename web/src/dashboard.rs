use dioxus::core::Task;
use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use serde_json::Value;

use crate::camera::CameraCard;
use crate::{Screen, State, api, http, icons, js, select_printer, show};

pub const SECTIONS: [(&str, &str); 7] = [
    ("jobCard", "Job status"),
    ("machineCard", "Machine status"),
    ("controlsCard", "Controls"),
    ("camCard", "Camera"),
    ("dryCard", "Bed drying"),
    ("nozzleCard", "Nozzle"),
    ("amsCard", "Filament (AMS)"),
];

const DRY_STOPS: [(i64, &str); 7] = [
    (0, "Off"),
    (60, "60°C · PLA"),
    (70, "70°C · PLA-CF/silk"),
    (80, "80°C · PETG"),
    (85, "85°C · TPU"),
    (90, "90°C · ABS/ASA/PC"),
    (100, "100°C · PA/PVA/desiccant"),
];

const NOZZLE_STOPS: [(i64, &str); 5] = [
    (0, "Off"),
    (225, "225°C · PLA"),
    (255, "255°C · PETG"),
    (275, "275°C · ABS/PC"),
    (295, "295°C · PA/CF"),
];

pub async fn load(mut state: State) {
    if let Some(settings) = http::get_json::<api::Settings>("/api/settings").await
        && !settings.dashboard.is_empty()
    {
        state
            .dashboard
            .set(settings.dashboard.split(',').map(str::to_string).collect());
    }
}

pub fn post(mut state: State, action: String) {
    let url = state.with_printer(&format!("/api/actions/{action}"));
    spawn(async move {
        let reply = match http::post(&url).await {
            Ok(response) => response.text().await.unwrap_or_default(),
            Err(_) => "request failed".to_string(),
        };
        state.log.set(reply);
    });
}

#[component]
pub fn MainScreen() -> Element {
    let state = use_context::<State>();
    let order = state.dashboard.cloned();
    rsx! {
        div { id: "mainScreen",
            div { class: "topbar",
                span { class: "screentitle", "BambuSpa" }
                button {
                    class: "topicon",
                    id: "eventsBtn",
                    "aria-label": "Events",
                    dangerous_inner_html: icons::EVENTS,
                    onclick: move |_| show(state, Screen::Events),
                }
                button {
                    class: "topicon",
                    id: "settingsBtn",
                    "aria-label": "Settings",
                    dangerous_inner_html: icons::SETTINGS,
                    onclick: move |_| show(state, Screen::Settings),
                }
            }
            Switcher {}
            HmsBanner {}
            for id in order {
                Section { key: "{id}", id: id.clone() }
            }
        }
    }
}

#[component]
fn Section(id: String) -> Element {
    match id.as_str() {
        "jobCard" => rsx! { JobCard {} },
        "machineCard" => rsx! { MachineCard {} },
        "controlsCard" => rsx! { Controls {} },
        "camCard" => rsx! { CameraCard {} },
        "dryCard" => rsx! { HeaterCard { heater: Heater::Bed } },
        "nozzleCard" => rsx! { HeaterCard { heater: Heater::Nozzle } },
        "amsCard" => rsx! { AmsCard {} },
        _ => rsx! {},
    }
}

#[component]
fn Switcher() -> Element {
    let state = use_context::<State>();
    let printers = state.printers.cloned();
    if printers.len() <= 1 {
        return rsx! {};
    }
    rsx! {
        div { id: "switcher", "role": "group", "aria-label": "Printer",
            for printer in printers {
                Chip { key: "{printer.id}", printer: printer.clone() }
            }
        }
    }
}

#[component]
fn Chip(printer: api::PrinterSummary) -> Element {
    let state = use_context::<State>();
    let pressed = state.current.cloned() == Some(printer.id);
    let title = if printer.connected {
        format!("{}: {}", printer.name, printer.gcode_state)
    } else {
        let problem = printer.problem.as_deref().unwrap_or("not connected");
        format!("{}: {}", printer.name, problem)
    };
    let dot = if printer.connected { "dot ok" } else { "dot bad" };
    let pct = (!printer.progress.is_null()).then(|| format!("{}%", js::text(&printer.progress)));
    let id = printer.id;
    let name = printer.name;
    rsx! {
        button {
            "aria-pressed": "{pressed}",
            title: "{title}",
            onclick: move |_| select_printer(state, id),
            span { class: dot }
            span { "{name}" }
            if let Some(pct) = pct {
                span { class: "pct", "{pct}" }
            }
        }
    }
}

#[component]
fn HmsBanner() -> Element {
    let state = use_context::<State>();
    let status = state.status.cloned();
    let Some(hms) = status.as_ref().map(|s| &s.hms).filter(|hms| !hms.is_empty()) else {
        return rsx! {};
    };
    let text = hms
        .iter()
        .map(|e| format!("⚠️ {}", e.message))
        .collect::<Vec<_>>()
        .join(" · ");
    rsx! {
        div {
            id: "hmsBanner",
            class: "card",
            style: "background:var(--alert-bg); color:var(--alert-text);",
            "{text}"
        }
    }
}

#[component]
fn JobCard() -> Element {
    let state = use_context::<State>();
    let status = state.status.cloned().unwrap_or_default();
    let progress = (!status.progress.is_null()).then(|| format!("{}%", js::text(&status.progress)));
    let job = js::truthy(&status.job_name).then(|| js::text(&status.job_name));
    let layers = js::number(&status.total_layer_num)
        .filter(|total| *total > 0.0)
        .map(|_| {
            format!(
                "{} / {}",
                js::or_unknown(&status.layer_num),
                js::text(&status.total_layer_num)
            )
        });
    let remaining = (!status.remaining_minutes.is_null())
        .then(|| js::fmt_minutes(js::number(&status.remaining_minutes).unwrap_or(f64::NAN)));
    let empty = progress.is_none() && job.is_none();
    rsx! {
        div { class: "card", id: "jobCard",
            h2 { "Job status" }
            if empty {
                div { class: "row", id: "jobEmpty", style: "color:var(--muted)", "No active print" }
            }
            if let Some(progress) = progress {
                div { class: "row", id: "progressRow",
                    span { "Progress" }
                    span { id: "progress", "{progress}" }
                }
            }
            if let Some(job) = job {
                div { class: "row", id: "jobRow",
                    span { "Job" }
                    span { id: "jobName", "{job}" }
                }
            }
            if let Some(layers) = layers {
                div { class: "row", id: "layerRow",
                    span { "Layer" }
                    span { id: "layer", "{layers}" }
                }
            }
            if let Some(remaining) = remaining {
                div { class: "row", id: "timeRow",
                    span { "Time remaining" }
                    span { id: "timeRemaining", "{remaining}" }
                }
            }
        }
    }
}

#[component]
fn MachineCard() -> Element {
    let state = use_context::<State>();
    let reachable = state.reachable.cloned();
    let status = state.status.cloned();
    let (conn, conn_class) = match &status {
        _ if !reachable => ("bridge unreachable".to_string(), "bad"),
        None => ("…".to_string(), ""),
        Some(s) if s.connected => ("connected".to_string(), "ok"),
        Some(s) if !s.configured => ("no printer set up (see Settings)".to_string(), "bad"),
        Some(s) => (
            s.problem
                .clone()
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| "connecting…".to_string()),
            "bad",
        ),
    };
    let (gcode, bed, nozzle, fans) = match &status {
        Some(s) => (
            s.gcode_state.clone(),
            format!("{} / {}°C", js::fixed1(&s.bed_temp), js::fixed1(&s.bed_target)),
            format!(
                "{} / {}°C",
                js::fixed1(&s.nozzle_temp),
                js::fixed1(&s.nozzle_target)
            ),
            format!(
                "{} / {} / {}",
                js::or_unknown(&s.fans.cooling),
                js::or_unknown(&s.fans.aux),
                js::or_unknown(&s.fans.chamber)
            ),
        ),
        None => ("…".into(), "…".into(), "…".into(), "…".into()),
    };
    let lamp_timer = status
        .as_ref()
        .and_then(|s| s.lamp_off_in)
        .filter(|_| reachable)
        .map(js::fmt_countdown);
    rsx! {
        div { class: "card", id: "machineCard",
            h2 { "Machine status" }
            div { class: "row",
                span { "Connection" }
                span { id: "conn", class: conn_class, "{conn}" }
            }
            div { class: "row",
                span { "State" }
                span { id: "state", "{gcode}" }
            }
            div { class: "row",
                span { "Bed" }
                span { id: "bed", "{bed}" }
            }
            div { class: "row",
                span { "Nozzle" }
                span { id: "nozzle", "{nozzle}" }
            }
            div { class: "row",
                span { "Fans (cool/aux/chamber)" }
                span { id: "fans", "{fans}" }
            }
            if let Some(lamp_timer) = lamp_timer {
                div {
                    class: "row",
                    id: "lampTimerRow",
                    style: "color:var(--accent); font-size:0.8rem",
                    span { "Lamp auto-off" }
                    span { id: "lampTimer", "{lamp_timer}" }
                }
            }
        }
    }
}

#[component]
fn Controls() -> Element {
    let state = use_context::<State>();
    let mut armed = use_signal(|| false);
    let mut disarm_timer = use_signal(|| None::<Task>);
    let mut disarm = move || {
        armed.set(false);
        let timer = *disarm_timer.peek();
        disarm_timer.set(None);
        if let Some(timer) = timer {
            timer.cancel();
        }
    };
    let status = state.status.cloned().filter(|_| state.reachable.cloned());
    let stop_allowed = status.as_ref().is_some_and(|s| s.print_actions.stop);
    use_effect(move || {
        let allowed = state.reachable.cloned()
            && matches!(&*state.status.read(), Some(s) if s.print_actions.stop);
        if !allowed {
            disarm();
        }
    });
    let s = status.unwrap_or_default();
    let enabled = state.reachable.cloned() && state.status.read().is_some();
    let actions = enabled && s.actions_allowed;
    let lamp_known = s.chamber_light.is_some();
    let lamp_on = s.chamber_light == Some(true);
    let lamp_action = if lamp_on { "lamp-off" } else { "lamp-on" };
    let lamp_cap = match s.chamber_light {
        Some(true) => "Lamp on",
        Some(false) => "Lamp off",
        None => "Lamp",
    };
    let paused = s.print_actions.resume;
    let pause_action = if paused { "resume" } else { "pause" };
    let pause_enabled = enabled
        && if paused {
            s.print_actions.resume
        } else {
            s.print_actions.pause
        };
    let extrude_hot = js::number(&s.nozzle_temp).unwrap_or(0.0) >= 170.0;
    let is_armed = armed.cloned();
    rsx! {
        div { id: "controlsCard",
            div { class: "iconrow cols3",
                button {
                    class: if lamp_on { "iconbtn on" } else { "iconbtn" },
                    id: "lampBtn",
                    "aria-label": if lamp_on { "Turn chamber lamp off" } else { "Turn chamber lamp on" },
                    disabled: !(enabled && s.connected && lamp_known),
                    onclick: move |_| post(state, lamp_action.to_string()),
                    span { class: "ico", dangerous_inner_html: icons::LAMP }
                    span { class: "cap", "{lamp_cap}" }
                }
                button {
                    class: "iconbtn",
                    id: "pauseBtn",
                    "aria-label": if paused { "Resume print" } else { "Pause print" },
                    disabled: !pause_enabled,
                    onclick: move |_| post(state, pause_action.to_string()),
                    span {
                        class: "ico",
                        dangerous_inner_html: if paused { icons::PLAY } else { icons::PAUSE },
                    }
                    span { class: "cap", if paused { "Resume" } else { "Pause" } }
                }
                button {
                    class: if is_armed { "iconbtn danger armed" } else { "iconbtn danger" },
                    id: "stopBtn",
                    "aria-label": "Stop print",
                    disabled: !(enabled && stop_allowed),
                    onclick: move |_| {
                        if *armed.peek() {
                            disarm();
                            post(state, "stop".to_string());
                        } else {
                            armed.set(true);
                            let timer = spawn(async move {
                                TimeoutFuture::new(4000).await;
                                armed.set(false);
                                disarm_timer.set(None);
                            });
                            disarm_timer.set(Some(timer));
                        }
                    },
                    span { class: "ico", dangerous_inner_html: icons::STOP }
                    span { class: "cap", if is_armed { "Confirm?" } else { "Stop" } }
                }
            }
            div { class: "iconrow",
                ActionButton {
                    action: "lower-bed",
                    label: "Lower bed",
                    caption: "Bed down",
                    icon: icons::LOWER_BED,
                    enabled: actions,
                }
                ActionButton {
                    action: "home",
                    label: "Home axes",
                    caption: "Home",
                    icon: icons::HOME,
                    enabled: actions,
                }
                ActionButton {
                    action: "extrude",
                    label: "Extrude filament",
                    caption: "Extrude",
                    icon: icons::EXTRUDE,
                    enabled: actions && extrude_hot,
                    id: "extrudeBtn",
                }
                ActionButton {
                    action: "unload",
                    label: "Unload loaded filament",
                    caption: "Unload",
                    icon: icons::UNLOAD,
                    enabled: actions,
                }
            }
        }
    }
}

#[component]
fn ActionButton(
    action: &'static str,
    label: &'static str,
    caption: &'static str,
    icon: &'static str,
    enabled: bool,
    id: Option<&'static str>,
) -> Element {
    let state = use_context::<State>();
    rsx! {
        button {
            class: "iconbtn",
            id,
            "aria-label": label,
            disabled: !enabled,
            onclick: move |_| post(state, action.to_string()),
            span { class: "ico", dangerous_inner_html: icon }
            span { class: "cap", "{caption}" }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Heater {
    Bed,
    Nozzle,
}

impl Heater {
    fn stops(self) -> &'static [(i64, &'static str)] {
        match self {
            Heater::Bed => &DRY_STOPS,
            Heater::Nozzle => &NOZZLE_STOPS,
        }
    }
}

#[component]
fn HeaterCard(heater: Heater) -> Element {
    let state = use_context::<State>();
    let mut dragging = use_signal(|| None::<usize>);
    let reachable = state.reachable.cloned();
    let Some(status) = state
        .status
        .cloned()
        .filter(|s| reachable && s.actions_allowed)
    else {
        return rsx! {};
    };
    let stops = heater.stops();
    let (target, off_in) = match heater {
        Heater::Bed => (&status.bed_target, status.bed_off_in),
        Heater::Nozzle => (&status.nozzle_target, status.nozzle_off_in),
    };
    let target = target.as_f64().unwrap_or(0.0);
    let mut nearest = 0;
    for (i, (t, _)) in stops.iter().enumerate() {
        if (*t as f64 - target).abs() < (stops[nearest].0 as f64 - target).abs() {
            nearest = i;
        }
    }
    let index = dragging.cloned().unwrap_or(nearest).min(stops.len() - 1);
    let label = stops[index].1;
    let max = stops.len() - 1;
    let countdown = off_in.map(js::fmt_countdown);
    let (card, title, row, slider, label_id, timer_row, timer, action) = match heater {
        Heater::Bed => (
            "dryCard",
            "Bed drying",
            "Bed target",
            "drySlider",
            "dryLabel",
            "dryTimerRow",
            "dryTimer",
            "set-bed-temp",
        ),
        Heater::Nozzle => (
            "nozzleCard",
            "Nozzle (cold pull / clean)",
            "Nozzle target",
            "nozzleSlider",
            "nozzleLabel",
            "nozzleTimerRow",
            "nozzleTimer",
            "set-nozzle-temp",
        ),
    };
    let style = (heater == Heater::Bed).then_some("margin-top:16px");
    rsx! {
        div { class: "card", id: card, style,
            h2 { "{title}" }
            div { class: "row",
                span { "{row}" }
                span { id: label_id, "{label}" }
            }
            input {
                r#type: "range",
                id: slider,
                min: "0",
                max: "{max}",
                step: "1",
                value: "{index}",
                oninput: move |e: FormEvent| dragging.set(e.value().parse().ok()),
                onchange: move |e: FormEvent| {
                    dragging.set(None);
                    let i: usize = e.value().parse().unwrap_or(0);
                    let temp = stops[i.min(stops.len() - 1)].0;
                    post(state, format!("{action}?temp={temp}"));
                },
            }
            if let Some(countdown) = countdown {
                div {
                    class: "row",
                    id: timer_row,
                    style: "color:var(--accent); font-size:0.8rem",
                    span { "Auto-off in" }
                    span { id: timer, "{countdown}" }
                }
            }
        }
    }
}

fn tray_target(unit_id: &Value, slot: &Value) -> Option<i64> {
    let u = js::parse_int(unit_id)?;
    let t = js::parse_int(&slot["id"]).filter(|t| (0..=3).contains(t))?;
    Some(u * 4 + t)
}

fn color6(slot: &Value) -> String {
    match slot["tray_color"].as_str() {
        Some(color) => color.chars().take(6).collect(),
        None => "666666".to_string(),
    }
}

fn humidity(unit: &Value) -> String {
    let mut parts = Vec::new();
    match &unit["humidity_raw"] {
        Value::Number(_) => parts.push(format!("{}%", js::text(&unit["humidity_raw"]))),
        Value::String(raw) if !raw.is_empty() => parts.push(format!("{raw}%")),
        _ => {}
    }
    let grade = match js::parse_int(&unit["humidity"]) {
        Some(5) => Some("A"),
        Some(4) => Some("B"),
        Some(3) => Some("C"),
        Some(2) => Some("D"),
        Some(1) => Some("E"),
        _ => None,
    };
    parts.extend(grade.map(str::to_string));
    parts.join(" · ")
}

#[component]
fn AmsCard() -> Element {
    let state = use_context::<State>();
    let reachable = state.reachable.cloned();
    let Some(status) = state.status.cloned() else {
        return rsx! {};
    };
    let units: Vec<Value> = status.ams["ams"]
        .as_array()
        .map(|units| {
            units
                .iter()
                .filter(|u| u["tray"].is_array())
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let has_trays = units
        .iter()
        .any(|u| u["tray"].as_array().is_some_and(|t| !t.is_empty()));
    if !has_trays {
        return rsx! {};
    }
    let loaded = js::parse_int(&status.ams["tray_now"]).filter(|n| *n < 254);
    let can_edit = reachable && status.actions_allowed;
    rsx! {
        div { class: "card", id: "amsCard",
            h2 { "Filament (AMS)" }
            div { id: "amsSlots",
                for (i, unit) in units.into_iter().enumerate() {
                    AmsUnit { key: "{i}", unit, loaded, can_edit }
                }
            }
        }
    }
}

#[component]
fn AmsUnit(unit: Value, loaded: Option<i64>, can_edit: bool) -> Element {
    let hum = humidity(&unit);
    let slots: Vec<Value> = unit["tray"]
        .as_array()
        .map(|t| t.iter().filter(|s| s.is_object()).cloned().collect())
        .unwrap_or_default();
    rsx! {
        if !hum.is_empty() {
            div { class: "row",
                span { "Humidity" }
                span { "{hum}" }
            }
        }
        for (i, slot) in slots.into_iter().enumerate() {
            AmsTray {
                key: "{i}",
                unit_id: unit["id"].clone(),
                slot,
                loaded,
                can_edit,
            }
        }
    }
}

#[component]
fn AmsTray(unit_id: Value, slot: Value, loaded: Option<i64>, can_edit: bool) -> Element {
    let mut open = use_signal(|| false);
    let target = tray_target(&unit_id, &slot);
    let tray_type = slot["tray_type"].as_str().unwrap_or("").to_string();
    let has_filament = !tray_type.is_empty();
    let label = if has_filament {
        tray_type
    } else {
        "empty".to_string()
    };
    let swatch = format!("background:#{}", color6(&slot));
    let is_loaded = has_filament && target == loaded;
    rsx! {
        div { class: "ams-slot",
            span { class: "ams-swatch", style: "{swatch}" }
            "{label}"
            span { style: "margin-left:auto; display:flex; align-items:center; gap:8px",
                if is_loaded {
                    span { style: "font-size:0.72rem; font-weight:600; color:var(--accent)",
                        "loaded"
                    }
                }
                button {
                    style: "padding:5px 12px; font-size:0.78rem; border-radius:8px",
                    disabled: !can_edit,
                    onclick: move |_| {
                        let now = !*open.peek();
                        open.set(now);
                    },
                    "Edit"
                }
            }
        }
        if open.cloned() {
            AmsEditor { unit_id, slot, open }
        }
    }
}

#[component]
fn AmsEditor(unit_id: Value, slot: Value, open: Signal<bool>) -> Element {
    let mut state = use_context::<State>();
    let mut open = open;
    let mut color = use_signal(|| format!("#{}", color6(&slot)));
    let mut kind = use_signal(|| slot["tray_type"].as_str().unwrap_or("").to_string());
    let given = |v: &Value| if v.is_null() { String::new() } else { js::text(v) };
    let mut temp_min = use_signal(|| given(&slot["nozzle_temp_min"]));
    let mut temp_max = use_signal(|| given(&slot["nozzle_temp_max"]));
    let save = move |_| {
        let kind = kind.peek().trim().to_string();
        if kind.is_empty() {
            state.log.set("type is required".to_string());
            return;
        }
        let or_zero = |v: String| if v.is_empty() { "0".to_string() } else { v };
        let color = color.peek().get(1..).unwrap_or("").to_string();
        let query = [
            ("ams_id", js::text(&unit_id)),
            ("tray_id", js::text(&slot["id"])),
            ("tray_color", format!("{color}FF")),
            ("tray_type", kind),
            ("nozzle_temp_min", or_zero(temp_min.peek().clone())),
            ("nozzle_temp_max", or_zero(temp_max.peek().clone())),
            (
                "tray_info_idx",
                slot["tray_info_idx"].as_str().unwrap_or("").to_string(),
            ),
        ]
        .iter()
        .map(|(k, v)| format!("{k}={}", js::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
        open.set(false);
        post(state, format!("set-filament?{query}"));
    };
    let (color_value, kind_value) = (color.cloned(), kind.cloned());
    let (min_value, max_value) = (temp_min.cloned(), temp_max.cloned());
    rsx! {
        div { class: "ams-editor",
            div { class: "ams-edit-row",
                label { "Color" }
                input {
                    r#type: "color",
                    value: "{color_value}",
                    oninput: move |e: FormEvent| color.set(e.value()),
                }
            }
            div { class: "ams-edit-row",
                label { "Type" }
                input {
                    r#type: "text",
                    maxlength: "20",
                    placeholder: "PLA",
                    value: "{kind_value}",
                    oninput: move |e: FormEvent| kind.set(e.value()),
                }
            }
            div { class: "ams-edit-row",
                label { "Temp °C" }
                span {
                    input {
                        r#type: "number",
                        min: "0",
                        max: "300",
                        class: "temp",
                        value: "{min_value}",
                        oninput: move |e: FormEvent| temp_min.set(e.value()),
                    }
                    " – "
                    input {
                        r#type: "number",
                        min: "0",
                        max: "300",
                        class: "temp",
                        value: "{max_value}",
                        oninput: move |e: FormEvent| temp_max.set(e.value()),
                    }
                }
            }
            button { class: "ams-save", onclick: save, "Save" }
        }
    }
}
