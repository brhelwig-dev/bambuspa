//! Push subscriptions and each device's notification preferences.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bambuspa_api::{
    EndpointRequest, Preferences, PreferencesRequest, PushKey, SubscribeRequest, TestResult,
};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;

use super::{App, Query, json_reply, read_json, text};
use crate::push::{KINDS, Notification};

pub async fn key(State(app): State<App>) -> Response {
    match app.sender.count() {
        Ok(subscribed) => json_reply(&PushKey {
            key: app.sender.public_key(),
            subscribed,
        }),
        Err(_) => text(
            StatusCode::INTERNAL_SERVER_ERROR,
            "cannot read subscriptions",
        ),
    }
}

pub async fn subscribe(State(app): State<App>, body: Bytes) -> Response {
    let Some(req) = read_json::<SubscribeRequest>(&body) else {
        return text(StatusCode::BAD_REQUEST, "invalid subscription");
    };
    let Ok(p256dh) = B64.decode(&req.keys.p256dh) else {
        return text(StatusCode::BAD_REQUEST, "subscription key is not base64url");
    };
    let Ok(auth) = B64.decode(&req.keys.auth) else {
        return text(StatusCode::BAD_REQUEST, "auth secret is not base64url");
    };
    let now = crate::clock::secs((app.clock)());
    match app.sender.subscribe(&req.endpoint, &p256dh, &auth, now) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => text(StatusCode::BAD_REQUEST, err),
    }
}

pub async fn unsubscribe(State(app): State<App>, body: Bytes) -> Response {
    let Some(req) = read_json::<EndpointRequest>(&body).filter(|r| !r.endpoint.is_empty()) else {
        return text(StatusCode::BAD_REQUEST, "invalid request");
    };
    match app.sender.unsubscribe(&req.endpoint) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => text(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

/// Proves the path — server, push service, phone — without waiting on the
/// printer. Goes to every device whatever it chose.
pub async fn test(State(app): State<App>) -> Response {
    match app
        .sender
        .send(&Notification::new(
            "BambuSpa",
            "Notifications are working.",
            "test",
            "",
        ))
        .await
    {
        Ok(delivered) => json_reply(&TestResult { delivered }),
        Err(err) => text(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

/// What one device asked for. The endpoint identifies the device, and only
/// that device knows its own.
pub async fn preferences(State(app): State<App>, query: Query) -> Response {
    let endpoint = query.get("endpoint").map(String::as_str).unwrap_or("");
    if endpoint.is_empty() {
        return text(StatusCode::BAD_REQUEST, "no endpoint");
    }
    match app.sender.find(endpoint) {
        Ok(Some(sub)) => {
            let all: Vec<String> = KINDS.iter().map(|k| k.to_string()).collect();
            json_reply(&Preferences {
                kinds: sub.kinds.unwrap_or_else(|| all.clone()),
                available: all,
                bed_interval: sub.bed_interval,
            })
        }
        Ok(None) => text(StatusCode::NOT_FOUND, "not subscribed"),
        Err(err) => text(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

pub async fn set_preferences(State(app): State<App>, body: Bytes) -> Response {
    let Some(req) = read_json::<PreferencesRequest>(&body).filter(|r| !r.endpoint.is_empty())
    else {
        return text(StatusCode::BAD_REQUEST, "invalid request");
    };
    match app.sender.set_preferences(
        &req.endpoint,
        &req.kinds.unwrap_or_default(),
        req.bed_interval,
    ) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => text(StatusCode::BAD_REQUEST, err),
    }
}
