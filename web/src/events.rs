use dioxus::prelude::*;

use crate::{Screen, State, api, http, icons, js, show};

async fn fetch(mut events: Signal<Option<Vec<api::Event>>>) {
    if let Some(reply) = http::get_json::<api::Events>("/api/events").await {
        events.set(Some(reply.events));
    }
}

#[component]
pub fn EventsScreen() -> Element {
    let state = use_context::<State>();
    let events = use_signal(|| None::<Vec<api::Event>>);
    let mut commands = use_signal(|| true);
    let mut reports = use_signal(|| false);
    let mut notifications = use_signal(|| true);
    use_hook(move || {
        spawn(fetch(events));
    });
    let wanted = move |e: &api::Event| match e.kind.as_str() {
        "command" => commands.cloned(),
        "report" => reports.cloned(),
        "notification" => notifications.cloned(),
        _ => true,
    };
    let list = events.cloned();
    let shown: Vec<api::Event> = list
        .iter()
        .flatten()
        .filter(|e| wanted(*e))
        .cloned()
        .collect();
    let empty = match &list {
        Some(all) if shown.is_empty() => Some(if all.is_empty() {
            "Nothing yet."
        } else {
            "Nothing of the kinds selected."
        }),
        _ => None,
    };
    rsx! {
        div { id: "eventsScreen",
            div { class: "topbar",
                button {
                    class: "topicon",
                    id: "eventsBack",
                    "aria-label": "Back",
                    dangerous_inner_html: icons::BACK,
                    onclick: move |_| show(state, Screen::Main),
                }
                span { class: "screentitle", "Events" }
                button {
                    class: "topicon",
                    id: "eventsRefresh",
                    "aria-label": "Refresh",
                    dangerous_inner_html: icons::REFRESH,
                    onclick: move |_| {
                        spawn(fetch(events));
                    },
                }
            }
            div { class: "card",
                div { class: "kindrow",
                    label { r#for: "eventsCommands", "Commands sent" }
                    input {
                        r#type: "checkbox",
                        id: "eventsCommands",
                        checked: commands.cloned(),
                        onchange: move |e: FormEvent| commands.set(e.checked()),
                    }
                }
                div { class: "kindrow",
                    label { r#for: "eventsReports", "Printer reports" }
                    input {
                        r#type: "checkbox",
                        id: "eventsReports",
                        checked: reports.cloned(),
                        onchange: move |e: FormEvent| reports.set(e.checked()),
                    }
                }
                div { class: "kindrow",
                    label { r#for: "eventsNotifications", "Notifications sent" }
                    input {
                        r#type: "checkbox",
                        id: "eventsNotifications",
                        checked: notifications.cloned(),
                        onchange: move |e: FormEvent| notifications.set(e.checked()),
                    }
                }
            }
            div { id: "eventsList",
                if let Some(empty) = empty {
                    div { class: "event", "{empty}" }
                }
                for event in shown {
                    EventRow { key: "{event.id}", event: event.clone() }
                }
            }
        }
    }
}

#[component]
fn EventRow(event: api::Event) -> Element {
    let state = use_context::<State>();
    let printers = state.printers.cloned();
    let about = event
        .printer
        .and_then(|id| printers.iter().find(|p| p.id == id))
        .filter(|_| printers.len() > 1);
    let what = match about {
        Some(p) => format!("{}: {}", p.name, event.summary),
        None => event.summary.clone(),
    };
    let when = js::local_time(&event.at);
    let ack = if event.kind == "report" {
        None
    } else if !event.error.is_empty() {
        Some(("ack bad", event.error.clone()))
    } else if let Some(acked) = &event.acked {
        Some(("ack yes", format!("acknowledged {}", js::local_time(acked))))
    } else {
        Some(("ack no", "waiting for acknowledgement".to_string()))
    };
    let payload = (!event.payload.is_empty()).then(|| event.payload.clone());
    rsx! {
        div { class: "event",
            div { class: "head",
                span { class: "what", "{what}" }
                span { class: "when", "{when}" }
            }
            if let Some((class, text)) = ack {
                div { class, "{text}" }
            }
            if let Some(payload) = payload {
                details {
                    summary { "raw" }
                    pre { "{payload}" }
                }
            }
        }
    }
}
