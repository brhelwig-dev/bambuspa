use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use dioxus::prelude::*;
use js_sys::{Promise, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Notification, NotificationPermission, PushSubscription, PushSubscriptionOptionsInit,
    ServiceWorkerRegistration,
};

use crate::{State, api, http, js};

const INTERVALS: [(i64, &str); 6] = [
    (0, "Never"),
    (3600, "Every hour"),
    (14400, "Every 4 hours"),
    (28800, "Every 8 hours"),
    (43200, "Every 12 hours"),
    (86400, "Every 24 hours"),
];

#[derive(Clone, Copy, PartialEq)]
enum Support {
    Starting,
    Unsupported,
    Failed,
    Ready,
}

#[derive(Clone, Copy)]
pub struct Push {
    support: Signal<Support>,
    registration: Signal<Option<ServiceWorkerRegistration>>,
    subscription: Signal<Option<PushSubscription>>,
    busy: Signal<bool>,
}

impl Push {
    pub fn new() -> Push {
        Push {
            support: Signal::new(Support::Starting),
            registration: Signal::new(None),
            subscription: Signal::new(None),
            busy: Signal::new(false),
        }
    }
}

fn has(target: &JsValue, key: &str) -> bool {
    Reflect::has(target, &JsValue::from_str(key)).unwrap_or(false)
}

fn installed() -> bool {
    let window = js::window();
    let standalone = window
        .match_media("(display-mode: standalone)")
        .ok()
        .flatten()
        .is_some_and(|query| query.matches());
    standalone
        || Reflect::get(&window.navigator(), &JsValue::from_str("standalone"))
            .ok()
            .and_then(|v| v.as_bool())
            == Some(true)
}

fn error_text(e: JsValue) -> String {
    Reflect::get(&e, &JsValue::from_str("message"))
        .ok()
        .and_then(|m| m.as_string())
        .filter(|m| !m.is_empty())
        .or_else(|| e.as_string())
        .unwrap_or_else(|| format!("{e:?}"))
}

async fn wait(promise: Result<Promise, JsValue>) -> Result<JsValue, String> {
    JsFuture::from(promise.map_err(error_text)?)
        .await
        .map_err(error_text)
}

async fn server_key() -> Result<String, String> {
    http::get_json::<api::PushKey>("/api/push/key")
        .await
        .map(|k| k.key)
        .ok_or_else(|| "no key".to_string())
}

fn subscription_key(sub: &PushSubscription) -> Option<String> {
    let buffer = sub.options().application_server_key().ok()??;
    Some(B64.encode(Uint8Array::new(&buffer).to_vec()))
}

async fn send_subscription(sub: &PushSubscription) -> Result<(), String> {
    let body = js_sys::JSON::stringify(sub).map_err(error_text)?;
    http::post_encoded_json("/api/push/subscribe", body.into()).await?;
    Ok(())
}

async fn init() -> Result<(ServiceWorkerRegistration, Option<PushSubscription>), String> {
    let container = js::window().navigator().service_worker();
    let registration: ServiceWorkerRegistration = wait(Ok(container.register("/sw.js")))
        .await?
        .unchecked_into();
    let manager = registration.push_manager().map_err(error_text)?;
    let found = wait(manager.get_subscription()).await?;
    if found.is_null() || found.is_undefined() {
        return Ok((registration, None));
    }
    let sub: PushSubscription = found.unchecked_into();
    if subscription_key(&sub) == Some(server_key().await?) {
        send_subscription(&sub).await?;
        Ok((registration, Some(sub)))
    } else {
        wait(sub.unsubscribe()).await?;
        Ok((registration, None))
    }
}

pub async fn start(mut push: Push) {
    let window = js::window();
    let ready = has(&window.navigator(), "serviceWorker")
        && has(&window, "PushManager")
        && has(&window, "Notification");
    if !ready {
        push.support.set(Support::Unsupported);
        return;
    }
    match init().await {
        Ok((registration, sub)) => {
            push.registration.set(Some(registration));
            push.subscription.set(sub);
            push.support.set(Support::Ready);
        }
        Err(_) => push.support.set(Support::Failed),
    }
}

async fn enable(mut push: Push, permission: Result<Promise, JsValue>) -> Result<(), String> {
    let permission = wait(permission).await?;
    if permission.as_string().as_deref() != Some("granted") {
        return Ok(());
    }
    let registration = push
        .registration
        .peek()
        .clone()
        .ok_or_else(|| "no service worker".to_string())?;
    let key = B64
        .decode(server_key().await?)
        .map_err(|e| e.to_string())?;
    let options = PushSubscriptionOptionsInit::new();
    options.set_user_visible_only(true);
    options.set_application_server_key(&Uint8Array::from(key.as_slice()));
    let manager = registration.push_manager().map_err(error_text)?;
    let sub: PushSubscription = wait(manager.subscribe_with_options(&options))
        .await?
        .unchecked_into();
    send_subscription(&sub).await?;
    push.subscription.set(Some(sub));
    Ok(())
}

async fn disable(mut push: Push, sub: PushSubscription) -> Result<(), String> {
    let endpoint = sub.endpoint();
    wait(sub.unsubscribe()).await?;
    push.subscription.set(None);
    http::post_json("/api/push/unsubscribe", &api::EndpointRequest { endpoint }).await?;
    Ok(())
}

fn toggle(mut push: Push, mut state: State) {
    push.busy.set(true);
    let subscribed = push.subscription.peek().clone();
    let permission = subscribed
        .is_none()
        .then(Notification::request_permission);
    spawn(async move {
        let result = match (subscribed, permission) {
            (Some(sub), _) => disable(push, sub).await,
            (None, Some(permission)) => enable(push, permission).await,
            (None, None) => Ok(()),
        };
        if let Err(e) = result {
            state.log.set(format!("notifications: {e}"));
        }
        push.busy.set(false);
    });
}

fn test(mut state: State, mut testing: Signal<bool>) {
    testing.set(true);
    spawn(async move {
        let reply = match http::post("/api/push/test").await {
            Ok(response) => response
                .json::<api::TestResult>()
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };
        let text = match reply {
            Ok(api::TestResult { delivered: 0 }) => "test reached no device".to_string(),
            Ok(api::TestResult { delivered }) => format!(
                "test sent to {delivered} device{}",
                if delivered == 1 { "" } else { "s" }
            ),
            Err(e) => format!("test failed: {e}"),
        };
        state.log.set(text);
        testing.set(false);
    });
}

fn save_prefs(push: Push, prefs: Signal<Option<api::Preferences>>, mut message: Signal<String>) {
    let Some(sub) = push.subscription.peek().clone() else {
        return;
    };
    let Some(chosen) = prefs.peek().clone() else {
        return;
    };
    let body = api::PreferencesRequest {
        endpoint: sub.endpoint(),
        kinds: Some(chosen.kinds),
        bed_interval: chosen.bed_interval,
    };
    spawn(async move {
        let text = match http::post_json("/api/push/preferences", &body).await {
            Ok(response) if response.ok() => "Saved.".to_string(),
            Ok(response) => http::reply_text(response).await,
            Err(e) => format!("could not save: {e}"),
        };
        message.set(text);
    });
}

fn kind_label(kind: &str) -> &str {
    match kind {
        "print-started" => "Print started",
        "print-finished" => "Print finished",
        "print-ended" => "Print ended without finishing",
        "printer-error" => "Printer error",
        "heater-off" => "Heater turned off automatically",
        other => other,
    }
}

#[component]
pub fn NotificationsCard(message: Signal<String>) -> Element {
    let state = use_context::<State>();
    let push = use_context::<Push>();
    let mut prefs = use_signal(|| None::<api::Preferences>);
    let testing = use_signal(|| false);
    use_effect(move || {
        let sub = push.subscription.cloned();
        spawn(async move {
            let Some(sub) = sub else {
                return;
            };
            let url = format!(
                "/api/push/preferences?endpoint={}",
                js::encode(&sub.endpoint())
            );
            if let Some(loaded) = http::get_json::<api::Preferences>(&url).await {
                prefs.set(Some(loaded));
            }
        });
    });
    let support = push.support.cloned();
    let subscribed = push.subscription.read().is_some();
    let blocked =
        support == Support::Ready && Notification::permission() == NotificationPermission::Denied;
    let state_text = match support {
        Support::Starting => "…",
        Support::Unsupported => "Not available here",
        Support::Failed => "Could not start notifications",
        Support::Ready if subscribed => "On for this device",
        Support::Ready if blocked => "Blocked in browser settings",
        Support::Ready => "Off",
    };
    let hint = match support {
        Support::Unsupported if installed() => Some("This browser cannot receive notifications."),
        Support::Unsupported => Some(
            "On iPhone, add this page to the Home Screen (Share → Add to Home Screen) and open it from there.",
        ),
        Support::Ready if blocked => {
            Some("Allow notifications for this site in your browser settings, then reload.")
        }
        _ => None,
    };
    let ready = support == Support::Ready;
    let chosen = prefs.cloned();
    let available = chosen.as_ref().map(|p| p.available.clone()).unwrap_or_default();
    let bed_interval = chosen.as_ref().map_or(0, |p| p.bed_interval);
    rsx! {
        div { class: "card",
            h2 { "Notifications" }
            div { class: "row",
                span { id: "notifyState", "{state_text}" }
            }
            div { class: "notifywhat",
                "Chosen per device, so this phone and anything else you use can want different things."
            }
            if ready && subscribed {
                div { id: "notifyPrefs",
                    div { id: "notifyKinds",
                        for kind in available {
                            KindRow {
                                key: "{kind}",
                                kind: kind.clone(),
                                prefs,
                                message,
                            }
                        }
                    }
                    div { class: "setrow",
                        label { r#for: "notifyBedInterval", "Bed on reminder" }
                        select {
                            id: "notifyBedInterval",
                            onchange: move |e: FormEvent| {
                                let interval = e.value().parse().unwrap_or(0);
                                if let Some(p) = &mut *prefs.write() {
                                    p.bed_interval = interval;
                                }
                                save_prefs(push, prefs, message);
                            },
                            for (value, label) in INTERVALS {
                                option {
                                    key: "{value}",
                                    value: "{value}",
                                    selected: value == bed_interval,
                                    "{label}"
                                }
                            }
                        }
                    }
                }
            }
            div { class: "notifybtns",
                if ready && !blocked {
                    button {
                        id: "notifyBtn",
                        disabled: push.busy.cloned(),
                        onclick: move |_| toggle(push, state),
                        if subscribed { "Turn off" } else { "Turn on" }
                    }
                }
                if ready && subscribed {
                    button {
                        id: "notifyTestBtn",
                        disabled: testing.cloned(),
                        onclick: move |_| test(state, testing),
                        "Send a test"
                    }
                }
            }
            if let Some(hint) = hint {
                div {
                    id: "notifyHint",
                    style: "color:var(--muted); font-size:0.8rem; margin-top:8px",
                    "{hint}"
                }
            }
        }
    }
}

#[component]
fn KindRow(
    kind: String,
    prefs: Signal<Option<api::Preferences>>,
    message: Signal<String>,
) -> Element {
    let push = use_context::<Push>();
    let mut prefs = prefs;
    let checked = prefs.read().iter().any(|p| p.kinds.contains(&kind));
    let id = format!("kind-{kind}");
    let label = kind_label(&kind).to_string();
    rsx! {
        div { class: "kindrow",
            label { r#for: "{id}", "{label}" }
            input {
                r#type: "checkbox",
                id: "{id}",
                value: "{kind}",
                checked,
                onchange: move |e: FormEvent| {
                    let on = e.checked();
                    if let Some(p) = &mut *prefs.write() {
                        let before = p.kinds.clone();
                        p.kinds = p
                            .available
                            .iter()
                            .filter(|k| if **k == kind { on } else { before.contains(k) })
                            .cloned()
                            .collect();
                    }
                    save_prefs(push, prefs, message);
                },
            }
        }
    }
}
