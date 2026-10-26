use dioxus::prelude::*;
use gloo_net::http::Request;

use crate::dashboard::SECTIONS;
use crate::push::NotificationsCard;
use crate::{Screen, State, api, http, icons, js, select_printer, show};

struct Field {
    id: &'static str,
    name: &'static str,
    per_unit: f64,
    label: &'static str,
    min: &'static str,
    max: &'static str,
    step: &'static str,
    unit: &'static str,
}

const FIELDS: [Field; 6] = [
    Field {
        id: "setTimelapseStorage",
        name: api::settings::TIMELAPSE_STORAGE,
        per_unit: 1024.0,
        label: "Timelapses kept on the SD card",
        min: "0",
        max: "1024",
        step: "0.5",
        unit: "GB (0 keeps all)",
    },
    Field {
        id: "setBedOff",
        name: api::settings::BED_OFF_AFTER,
        per_unit: 3600.0,
        label: "Bed heater auto-off",
        min: "1",
        max: "168",
        step: "1",
        unit: "hours",
    },
    Field {
        id: "setNozzleOff",
        name: api::settings::NOZZLE_OFF_AFTER,
        per_unit: 60.0,
        label: "Nozzle heater auto-off",
        min: "1",
        max: "10080",
        step: "1",
        unit: "minutes",
    },
    Field {
        id: "setLampOff",
        name: api::settings::LAMP_OFF_AFTER,
        per_unit: 3600.0,
        label: "Chamber light auto-off",
        min: "1",
        max: "168",
        step: "1",
        unit: "hours idle",
    },
    Field {
        id: "setActivityLimit",
        name: api::settings::ACTIVITY_LIMIT,
        per_unit: 1.0,
        label: "Event log",
        min: "1",
        max: "512",
        step: "1",
        unit: "MB",
    },
    Field {
        id: "setSessionLength",
        name: api::settings::SESSION_LENGTH,
        per_unit: 86400.0,
        label: "Stay logged in",
        min: "1",
        max: "365",
        step: "1",
        unit: "days",
    },
];

fn stored(settings: &api::Settings, name: &str) -> i64 {
    match name {
        api::settings::TIMELAPSE_STORAGE => settings.timelapse_storage,
        api::settings::BED_OFF_AFTER => settings.bed_off_after,
        api::settings::NOZZLE_OFF_AFTER => settings.nozzle_off_after,
        api::settings::LAMP_OFF_AFTER => settings.lamp_off_after,
        api::settings::ACTIVITY_LIMIT => settings.activity_limit,
        _ => settings.session_length,
    }
}

fn entered(text: &str) -> f64 {
    let text = text.trim();
    if text.is_empty() {
        0.0
    } else {
        text.parse().unwrap_or(f64::NAN)
    }
}

fn shown(value: f64) -> String {
    js_sys::Number::from(value)
        .to_fixed(2)
        .ok()
        .and_then(|fixed| String::from(fixed).parse::<f64>().ok())
        .map(|n| n.to_string())
        .unwrap_or_default()
}

#[component]
pub fn SettingsScreen() -> Element {
    let state = use_context::<State>();
    let message = use_signal(String::new);
    rsx! {
        div { id: "settingsScreen",
            div { class: "topbar",
                button {
                    class: "topicon",
                    id: "settingsBack",
                    "aria-label": "Back",
                    dangerous_inner_html: icons::BACK,
                    onclick: move |_| show(state, Screen::Main),
                }
                span { class: "screentitle", "Settings" }
            }
            NotificationsCard { message }
            Appearance {}
            DashboardCard { message }
            PrintersCard {}
            GeneralCard { message }
            AccountCard {}
        }
    }
}

fn apply_theme(choice: &str) {
    if let Some(root) = js::window().document().and_then(|d| d.document_element()) {
        let _ = if choice == "light" || choice == "dark" {
            root.set_attribute("data-theme", choice)
        } else {
            root.remove_attribute("data-theme")
        };
    }
    js::store("theme", choice);
}

#[component]
fn Appearance() -> Element {
    let mut theme = use_signal(|| js::stored("theme").unwrap_or_else(|| "system".to_string()));
    let current = theme.cloned();
    rsx! {
        div { class: "card",
            h2 { "Appearance" }
            div { class: "setrow",
                label { r#for: "setTheme", "Theme" }
                select {
                    id: "setTheme",
                    onchange: move |e: FormEvent| {
                        let choice = e.value();
                        apply_theme(&choice);
                        theme.set(choice);
                    },
                    for (value, label) in [("system", "Follow system"), ("light", "Light"), ("dark", "Dark")] {
                        option { key: "{value}", value, selected: current == value, "{label}" }
                    }
                }
            }
        }
    }
}

fn save_dashboard(mut state: State, order: Vec<String>, mut message: Signal<String>) {
    let url = format!(
        "/api/settings/dashboard?text={}",
        js::encode(&order.join(","))
    );
    state.dashboard.set(order);
    spawn(async move {
        let text = match http::post(&url).await {
            Ok(response) if response.ok() => "Saved.".to_string(),
            Ok(response) => http::reply_text(response).await,
            Err(e) => format!("could not save: {e}"),
        };
        message.set(text);
    });
}

#[component]
fn DashboardCard(message: Signal<String>) -> Element {
    let state = use_context::<State>();
    let order = state.dashboard.cloned();
    let known = |id: &str| SECTIONS.iter().any(|(s, _)| *s == id);
    let mut rows: Vec<String> = order.iter().filter(|id| known(id)).cloned().collect();
    rows.extend(
        SECTIONS
            .iter()
            .map(|(id, _)| id.to_string())
            .filter(|id| !order.contains(id)),
    );
    let count = rows.len();
    rsx! {
        div { class: "card",
            h2 { "Dashboard" }
            div { id: "dashList",
                for (index, id) in rows.clone().into_iter().enumerate() {
                    DashRow {
                        key: "{id}",
                        id: id.clone(),
                        first: index == 0,
                        last: index + 1 == count,
                        rows: rows.clone(),
                        message,
                    }
                }
            }
            div { class: "notifywhat",
                "Unticked sections are hidden. The arrows move a section up or down the page."
            }
        }
    }
}

#[component]
fn DashRow(
    id: String,
    first: bool,
    last: bool,
    rows: Vec<String>,
    message: Signal<String>,
) -> Element {
    let state = use_context::<State>();
    let label = SECTIONS
        .iter()
        .find(|(s, _)| *s == id)
        .map(|(_, label)| *label)
        .unwrap_or("");
    let checked = state.dashboard.read().contains(&id);
    let box_id = format!("dash-{id}");
    let shift = {
        let id = id.clone();
        move |by: isize| {
            let mut order = state.dashboard.cloned();
            let Some(at) = order.iter().position(|o| *o == id) else {
                return;
            };
            let to = at as isize + by;
            if to < 0 || to as usize >= order.len() {
                return;
            }
            order.swap(at, to as usize);
            save_dashboard(state, order, message);
        }
    };
    let up = shift.clone();
    let down = shift;
    rsx! {
        div { class: "dashrow",
            label { r#for: "{box_id}", "{label}" }
            button {
                class: "move",
                "aria-label": "Move {label} up",
                disabled: first,
                onclick: move |_| up(-1),
                "↑"
            }
            button {
                class: "move",
                "aria-label": "Move {label} down",
                disabled: last,
                onclick: move |_| down(1),
                "↓"
            }
            input {
                r#type: "checkbox",
                id: "{box_id}",
                checked,
                onchange: move |e: FormEvent| {
                    let order = state.dashboard.cloned();
                    let order = if e.checked() {
                        rows.iter()
                            .filter(|other| **other == id || order.contains(other))
                            .cloned()
                            .collect()
                    } else {
                        order.into_iter().filter(|other| *other != id).collect()
                    };
                    save_dashboard(state, order, message);
                },
            }
        }
    }
}

async fn load_printers(mut rows: Signal<Vec<api::PrinterInfo>>) {
    if let Some(list) = http::get_json::<Vec<api::PrinterInfo>>("/api/printers").await {
        rows.set(list);
    }
}

#[derive(Clone, Copy, PartialEq)]
struct PrinterForm {
    open: Signal<bool>,
    editing: Signal<Option<i64>>,
    name: Signal<String>,
    ip: Signal<String>,
    serial: Signal<String>,
    code: Signal<String>,
    saving: Signal<bool>,
    anyway: Signal<bool>,
    message: Signal<(String, bool)>,
}

impl PrinterForm {
    fn show(mut self, printer: Option<&api::PrinterInfo>) {
        self.editing.set(printer.map(|p| p.id));
        self.name
            .set(printer.map(|p| p.name.clone()).unwrap_or_default());
        self.ip
            .set(printer.map(|p| p.ip.clone()).unwrap_or_default());
        self.serial
            .set(printer.map(|p| p.serial.clone()).unwrap_or_default());
        self.code.set(String::new());
        self.anyway.set(false);
        self.message.set((String::new(), false));
        self.open.set(true);
    }

    fn close(mut self) {
        self.editing.set(None);
        self.open.set(false);
    }
}

fn save_printer(
    state: State,
    mut form: PrinterForm,
    rows: Signal<Vec<api::PrinterInfo>>,
    skip_check: bool,
) {
    form.saving.set(true);
    form.anyway.set(false);
    let editing = *form.editing.peek();
    let old = rows.peek().iter().find(|p| Some(p.id) == editing).cloned();
    let body = api::PrinterRequest {
        name: form.name.peek().trim().to_string(),
        ip: form.ip.peek().trim().to_string(),
        serial: form.serial.peek().trim().to_string(),
        access_code: form.code.peek().clone(),
        skip_check,
    };
    let details_changed = match &old {
        None => true,
        Some(old) => !body.access_code.is_empty() || body.ip != old.ip || body.serial != old.serial,
    };
    let quick = skip_check || !details_changed;
    let progress = if quick {
        "Saving…"
    } else {
        "Checking the printer answers…"
    };
    form.message.set((progress.to_string(), false));
    let url = match editing {
        Some(id) => format!("/api/printers/{id}"),
        None => "/api/printers".to_string(),
    };
    spawn(async move {
        match http::post_json(&url, &body).await {
            Ok(response) if response.ok() => {
                if editing.is_none()
                    && let Ok(created) = response.json::<api::Created>().await
                {
                    select_printer(state, created.id);
                }
                form.close();
                load_printers(rows).await;
                let done = if quick {
                    format!("Saved {}.", body.name)
                } else {
                    format!("Connected to {} and saved.", body.name)
                };
                form.message.set((done, false));
            }
            Ok(response) => {
                let refused = response.status() == 422;
                form.message.set((http::reply_text(response).await, true));
                if refused {
                    form.anyway.set(true);
                }
            }
            Err(e) => form.message.set((format!("could not save: {e}"), true)),
        }
        form.saving.set(false);
    });
}

fn remove_printer(
    mut form: PrinterForm,
    rows: Signal<Vec<api::PrinterInfo>>,
    printer: &api::PrinterInfo,
) {
    let question = format!("Remove {}?", printer.name);
    if !js::window()
        .confirm_with_message(&question)
        .unwrap_or(false)
    {
        return;
    }
    let (id, name) = (printer.id, printer.name.clone());
    spawn(async move {
        match http::delete(&format!("/api/printers/{id}")).await {
            Ok(response) if response.ok() => form.message.set((format!("Removed {name}."), false)),
            Ok(response) => form.message.set((http::reply_text(response).await, true)),
            Err(e) => form
                .message
                .with_mut(|m| m.0 = format!("could not remove: {e}")),
        }
        form.close();
        load_printers(rows).await;
    });
}

#[component]
fn PrintersCard() -> Element {
    let state = use_context::<State>();
    let rows = use_signal(Vec::<api::PrinterInfo>::new);
    let form = PrinterForm {
        open: use_signal(|| false),
        editing: use_signal(|| None),
        name: use_signal(String::new),
        ip: use_signal(String::new),
        serial: use_signal(String::new),
        code: use_signal(String::new),
        saving: use_signal(|| false),
        anyway: use_signal(|| false),
        message: use_signal(|| (String::new(), false)),
    };
    use_hook(move || {
        spawn(load_printers(rows));
    });
    let list = rows.cloned();
    let open = form.open.cloned();
    let editing = form.editing.cloned().is_some();
    let (text, bad) = form.message.cloned();
    let message_class = if bad { "notifywhat bad" } else { "notifywhat" };
    let (mut name, mut ip, mut serial, mut code) = (form.name, form.ip, form.serial, form.code);
    let values = (name.cloned(), ip.cloned(), serial.cloned(), code.cloned());
    rsx! {
        div { class: "card",
            h2 { "Printers" }
            div { id: "printerList",
                if list.is_empty() {
                    div { class: "notifywhat", "No printer set up yet." }
                }
                for printer in list {
                    PrinterItem { key: "{printer.id}", printer: printer.clone(), form, rows }
                }
            }
            if !open {
                button {
                    id: "printerAddBtn",
                    style: "margin-top:10px",
                    onclick: move |_| form.show(None),
                    "Add printer"
                }
            }
            if open {
                div { id: "printerForm", style: "margin-top:6px",
                    div { class: "setrow",
                        label { r#for: "setPrinterName", "Name" }
                        input {
                            r#type: "text",
                            id: "setPrinterName",
                            placeholder: "Garage P1S",
                            maxlength: "40",
                            autocomplete: "off",
                            value: "{values.0}",
                            oninput: move |e: FormEvent| name.set(e.value()),
                            onmounted: move |e: MountedEvent| async move {
                                let _ = e.set_focus(true).await;
                            },
                        }
                        span { class: "unit" }
                    }
                    div { class: "setrow",
                        label { r#for: "setPrinterIP", "Address" }
                        input {
                            r#type: "text",
                            id: "setPrinterIP",
                            placeholder: "192.0.2.10",
                            "inputmode": "decimal",
                            value: "{values.1}",
                            oninput: move |e: FormEvent| ip.set(e.value()),
                        }
                        span { class: "unit" }
                    }
                    div { class: "setrow",
                        label { r#for: "setPrinterSerial", "Serial" }
                        input {
                            r#type: "text",
                            id: "setPrinterSerial",
                            placeholder: "01P00A000000000",
                            value: "{values.2}",
                            oninput: move |e: FormEvent| serial.set(e.value()),
                        }
                        span { class: "unit" }
                    }
                    div { class: "setrow",
                        label { r#for: "setPrinterCode", "Access code" }
                        input {
                            r#type: "password",
                            id: "setPrinterCode",
                            placeholder: if editing { "unchanged" } else { "required" },
                            autocomplete: "off",
                            value: "{values.3}",
                            oninput: move |e: FormEvent| code.set(e.value()),
                        }
                        span { class: "unit" }
                    }
                    div { class: "notifywhat",
                        "On the printer: Settings → WLAN for the address and access code, Settings → Device for the serial."
                    }
                    div { class: "formbtns",
                        button {
                            id: "printerSave",
                            disabled: form.saving.cloned(),
                            onclick: move |_| save_printer(state, form, rows, false),
                            if editing { "Save" } else { "Add printer" }
                        }
                        button {
                            id: "printerCancel",
                            onclick: move |_| {
                                let mut message = form.message;
                                form.close();
                                message.set((String::new(), false));
                            },
                            "Cancel"
                        }
                    }
                    if form.anyway.cloned() {
                        button {
                            id: "printerSaveAnyway",
                            style: "margin-top:8px",
                            onclick: move |_| save_printer(state, form, rows, true),
                            "Save anyway"
                        }
                    }
                }
            }
            div { id: "printerMsg", class: message_class, style: "margin:8px 0 0", "{text}" }
        }
    }
}

#[component]
fn PrinterItem(
    printer: api::PrinterInfo,
    form: PrinterForm,
    rows: Signal<Vec<api::PrinterInfo>>,
) -> Element {
    let state = use_context::<State>();
    let live = state
        .printers
        .read()
        .iter()
        .find(|p| p.id == printer.id)
        .cloned();
    let detail = match &live {
        None => printer.ip.clone(),
        Some(p) if p.connected => format!("{} · connected", printer.ip),
        Some(p) => format!(
            "{} · {}",
            printer.ip,
            p.problem
                .clone()
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| "connecting…".to_string())
        ),
    };
    let detail_class = live.as_ref().filter(|p| !p.connected).map(|_| "bad");
    let name = printer.name.clone();
    let editing = printer.clone();
    rsx! {
        div { class: "printeritem",
            div { class: "who",
                div { "{name}" }
                div { class: detail_class, "{detail}" }
            }
            button { onclick: move |_| form.show(Some(&editing)), "Edit" }
            button { onclick: move |_| remove_printer(form, rows, &printer), "Remove" }
        }
    }
}

#[component]
fn GeneralCard(message: Signal<String>) -> Element {
    let saved = use_signal(|| None::<api::Settings>);
    let values = use_signal(|| vec![String::new(); FIELDS.len()]);
    let mut saving = use_signal(|| false);
    let load = move || async move {
        let (mut saved, mut values, mut message) = (saved, values, message);
        if let Some(settings) = http::get_json::<api::Settings>("/api/settings").await {
            values.set(
                FIELDS
                    .iter()
                    .map(|f| shown(stored(&settings, f.name) as f64 / f.per_unit))
                    .collect(),
            );
            saved.set(Some(settings));
            message.set(String::new());
        }
    };
    use_hook(move || {
        spawn(load());
    });
    let entered_value = move |i: usize| (entered(&values.read()[i]) * FIELDS[i].per_unit).round();
    let dirty = match &*saved.read() {
        Some(settings) => FIELDS
            .iter()
            .enumerate()
            .any(|(i, f)| entered_value(i) != stored(settings, f.name) as f64),
        None => true,
    };
    let save = move |_| {
        let Some(settings) = saved.peek().clone() else {
            return;
        };
        saving.set(true);
        let mut message = message;
        message.set("Saving…".to_string());
        let changes: Vec<(&str, f64)> = FIELDS
            .iter()
            .enumerate()
            .map(|(i, f)| (f.name, entered_value(i)))
            .filter(|(name, value)| *value != stored(&settings, name) as f64)
            .collect();
        spawn(async move {
            let mut failures = Vec::new();
            for (name, value) in changes {
                let value = if value.is_nan() {
                    "NaN".to_string()
                } else {
                    (value as i64).to_string()
                };
                match http::post(&format!("/api/settings/{name}?value={value}")).await {
                    Ok(response) if response.ok() => {}
                    Ok(response) => failures.push(http::reply_text(response).await),
                    Err(e) => failures.push(e),
                }
            }
            load().await;
            message.set(if failures.is_empty() {
                "Saved.".to_string()
            } else {
                failures.join(" · ")
            });
            saving.set(false);
        });
    };
    let text = message.cloned();
    let current = values.cloned();
    rsx! {
        div { class: "card",
            h2 { "General" }
            for (i, field) in FIELDS.iter().enumerate() {
                div { key: "{field.id}", class: "setrow",
                    label { r#for: field.id, "{field.label}" }
                    input {
                        r#type: "number",
                        id: field.id,
                        min: field.min,
                        max: field.max,
                        step: field.step,
                        value: current[i].clone(),
                        oninput: move |e: FormEvent| {
                            let mut values = values;
                            values.write()[i] = e.value();
                        },
                    }
                    span { class: "unit", "{field.unit}" }
                }
            }
            button {
                id: "settingsSave",
                disabled: !dirty || saving.cloned(),
                onclick: save,
                "Save"
            }
            div { id: "settingsMsg", class: "notifywhat", style: "margin:8px 0 0", "{text}" }
        }
    }
}

async fn log_out() {
    let mut then = "/".to_string();
    if let Ok(response) = Request::post("/auth/logout").send().await
        && response.ok()
        && let Ok(reply) = response.json::<api::Logout>().await
        && !reply.then.is_empty()
    {
        then = reply.then;
    }
    js::go_to(&then);
}

#[component]
fn AccountCard() -> Element {
    let mut who = use_signal(|| None::<String>);
    use_hook(move || {
        spawn(async move {
            if let Ok(response) = Request::get("/auth/me").send().await
                && response.ok()
                && let Ok(me) = response.json::<api::Me>().await
            {
                let name = if me.name.is_empty() {
                    me.subject
                } else {
                    me.name
                };
                who.set(Some(name));
            }
        });
    });
    let Some(name) = who.cloned() else {
        return rsx! {};
    };
    rsx! {
        div { class: "card", id: "accountCard",
            h2 { "Account" }
            div { class: "notifywhat", id: "accountWho", "Signed in as {name}" }
            button {
                id: "logout",
                onclick: move |_| {
                    spawn(log_out());
                },
                "Log out"
            }
        }
    }
}
