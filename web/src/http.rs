use std::cell::Cell;

use gloo_net::http::{Request, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::js;

thread_local! {
    static LOGGING_IN: Cell<bool> = const { Cell::new(false) };
}

fn checked(response: Result<Response, gloo_net::Error>) -> Result<Response, String> {
    let response = response.map_err(|e| e.to_string())?;
    if response.status() == 401 && !LOGGING_IN.replace(true) {
        js::go_to(&format!("/auth/login?next={}", js::encode(&js::here())));
    }
    Ok(response)
}

pub async fn get(url: &str) -> Result<Response, String> {
    checked(Request::get(url).send().await)
}

pub async fn post(url: &str) -> Result<Response, String> {
    checked(Request::post(url).send().await)
}

pub async fn delete(url: &str) -> Result<Response, String> {
    checked(Request::delete(url).send().await)
}

pub async fn post_json(url: &str, body: &impl Serialize) -> Result<Response, String> {
    let request = Request::post(url).json(body).map_err(|e| e.to_string())?;
    checked(request.send().await)
}

pub async fn post_encoded_json(url: &str, body: String) -> Result<Response, String> {
    let request = Request::post(url)
        .header("Content-Type", "application/json")
        .body(body)
        .map_err(|e| e.to_string())?;
    checked(request.send().await)
}

pub async fn get_json<T: DeserializeOwned>(url: &str) -> Option<T> {
    let response = get(url).await.ok()?;
    if !response.ok() {
        return None;
    }
    response.json().await.ok()
}

pub async fn reply_text(response: Response) -> String {
    response
        .text()
        .await
        .map(|t| t.trim().to_string())
        .unwrap_or_default()
}
