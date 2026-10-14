//! The page and the files a phone fetches alongside it, built into the binary.

use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use super::text;

#[derive(RustEmbed)]
#[folder = "static/"]
struct Static;

pub async fn serve(method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        let mut resp = text(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
        resp.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return resp;
    }
    let path = uri.path();
    if path == "/index.html" {
        return (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, "./")]).into_response();
    }
    let name = if path == "/" {
        "index.html"
    } else {
        path.trim_start_matches('/')
    };
    let Some(file) = Static::get(name) else {
        return text(StatusCode::NOT_FOUND, "404 page not found");
    };
    let mime = match name.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("webmanifest") => "application/manifest+json",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    };
    // Embedded files carry no modification time, so they are served no-cache:
    // revalidating is cheap, and a stale page on a phone is worse.
    let mut resp = file.data.into_owned().into_response();
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(etag) =
        HeaderValue::from_str(&format!("\"{}\"", hex(&file.metadata.sha256_hash()[..8])))
    {
        headers.insert(header::ETAG, etag);
    }
    resp
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
