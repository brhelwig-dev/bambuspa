use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bambuspa_api::{FRAME_COUNT, FRAME_INTERVAL};

use super::{App, Query, text};
use crate::printers::Printer;
use crate::timelapses::Error;
use std::sync::Arc;

fn pick(app: &App, query: &Query) -> Result<Arc<Printer>, Box<Response>> {
    match app.printer(query)? {
        Some(p) => Ok(p),
        None => Err(Box::new(text(StatusCode::NOT_FOUND, "no printer set up"))),
    }
}

fn failed(err: Error) -> Response {
    match err {
        Error::NotFound => text(StatusCode::NOT_FOUND, "no such timelapse"),
        Error::Failed(err) => {
            tracing::warn!("timelapses: {err}");
            text(StatusCode::BAD_GATEWAY, err)
        }
    }
}

fn jpeg(body: Vec<u8>) -> Response {
    let mut resp = body.into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    resp
}

pub async fn frame(State(app): State<App>, query: Query) -> Response {
    let printer = match pick(&app, &query) {
        Ok(p) => p,
        Err(refusal) => return *refusal,
    };
    let (Some(name), Some(i)) = (
        query.get("name"),
        query.get("i").and_then(|i| i.parse::<usize>().ok()),
    ) else {
        return text(StatusCode::BAD_REQUEST, "name and i are required");
    };
    let (video, i) = match printer.library.frame(name, i).await {
        Ok(found) => found,
        Err(err) => return failed(err),
    };
    let mut resp = jpeg(video.frame(i).unwrap_or_default().to_vec());
    let headers = resp.headers_mut();
    headers.insert(FRAME_COUNT, video.len().into());
    headers.insert(FRAME_INTERVAL, video.interval_us.into());
    resp
}

pub async fn thumbnail(State(app): State<App>, query: Query) -> Response {
    let printer = match pick(&app, &query) {
        Ok(p) => p,
        Err(refusal) => return *refusal,
    };
    let Some(name) = query.get("name") else {
        return text(StatusCode::BAD_REQUEST, "name is required");
    };
    match printer.library.thumbnail(name).await {
        Ok(body) => {
            let mut resp = jpeg(body);
            resp.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=86400"),
            );
            resp
        }
        Err(err) => failed(err),
    }
}
