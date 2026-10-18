//! The printers on the Settings screen: listing, adding, editing (including
//! renaming) and removing them. Access codes are never sent back, only
//! whether one is set.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bambuspa_api::{Created, PrinterInfo, PrinterRequest};

use super::{App, json_reply, read_json, text};
use crate::p1s::Config;

pub async fn list(State(app): State<App>) -> Response {
    let printers: Vec<_> = app
        .printers
        .list()
        .iter()
        .map(|p| {
            let conf = p.link.config();
            PrinterInfo {
                id: p.id,
                name: p.name(),
                ip: conf.ip,
                serial: conf.serial,
                access_code_set: !conf.access_code.is_empty(),
            }
        })
        .collect();
    json_reply(&printers)
}

fn config(req: &PrinterRequest, stored_code: &str) -> Result<Config, Box<Response>> {
    let mut access_code = req.access_code.trim().to_string();
    if access_code.is_empty() {
        access_code = stored_code.into();
    }
    let conf = Config {
        ip: req.ip.trim().into(),
        serial: req.serial.trim().into(),
        access_code,
    };
    if !conf.complete() {
        return Err(Box::new(text(
            StatusCode::BAD_REQUEST,
            "the printer\'s address, serial and access code are all needed",
        )));
    }
    if [&conf.ip, &conf.serial, &conf.access_code]
        .iter()
        .any(|v| v.len() > 256)
    {
        return Err(Box::new(text(StatusCode::BAD_REQUEST, "too long")));
    }
    Ok(conf)
}

/// Connects once with `conf`, unless asked not to, and says what is wrong
/// if the printer doesn't answer.
async fn check(req: &PrinterRequest, app: &App, conf: &Config) -> Result<(), Box<Response>> {
    if req.skip_check {
        return Ok(());
    }
    crate::p1s::mqtt::probe(
        &conf.ip,
        app.printers.mqtt_port(),
        &conf.serial,
        &conf.access_code,
    )
    .await
    .map_err(|problem| Box::new(text(StatusCode::UNPROCESSABLE_ENTITY, problem)))
}

pub async fn add(State(app): State<App>, body: Bytes) -> Response {
    let Some(req) = read_json::<PrinterRequest>(&body) else {
        return text(StatusCode::BAD_REQUEST, "invalid request");
    };
    let conf = match config(&req, "") {
        Ok(conf) => conf,
        Err(refusal) => return *refusal,
    };
    // A bad name is cheaper to find out about than an unanswered printer.
    if let Err(err) = app.printers.check_name(&req.name, None) {
        return text(StatusCode::BAD_REQUEST, err);
    }
    if let Err(refusal) = check(&req, &app, &conf).await {
        return *refusal;
    }
    match app.printers.add(&req.name, conf) {
        Ok(id) => (StatusCode::CREATED, json_reply(&Created { id })).into_response(),
        Err(err) => text(StatusCode::BAD_REQUEST, err),
    }
}

/// Renames a printer or changes its details. The printer is only checked when
/// its address, serial or access code changed.
pub async fn update(State(app): State<App>, Path(id): Path<i64>, body: Bytes) -> Response {
    let Some(printer) = app.printers.get(id) else {
        return text(StatusCode::NOT_FOUND, "no such printer");
    };
    let Some(req) = read_json::<PrinterRequest>(&body) else {
        return text(StatusCode::BAD_REQUEST, "invalid request");
    };
    let stored = printer.link.config();
    let conf = match config(&req, &stored.access_code) {
        Ok(conf) => conf,
        Err(refusal) => return *refusal,
    };
    if let Err(err) = app.printers.check_name(&req.name, Some(id)) {
        return text(StatusCode::BAD_REQUEST, err);
    }
    if conf != stored
        && let Err(refusal) = check(&req, &app, &conf).await
    {
        return *refusal;
    }
    match app.printers.update(id, &req.name, conf) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => text(StatusCode::BAD_REQUEST, err),
    }
}

pub async fn remove(State(app): State<App>, Path(id): Path<i64>) -> Response {
    if app.printers.get(id).is_none() {
        return text(StatusCode::NOT_FOUND, "no such printer");
    }
    match app.printers.remove(id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => text(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}
