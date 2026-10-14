//! Refuses unsafe requests that a browser says came from another origin, so a
//! web page open on the LAN can't post actions here. SameSite cookies don't
//! cover a sibling subdomain, and with login disabled there is no cookie at
//! all. The same rules as Go's net/http CrossOriginProtection.

use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

use super::text;

#[derive(Clone, Default)]
pub struct CrossOrigin {
    /// Origins allowed to post here besides the request's own host, e.g.
    /// "https://printer.example.com".
    pub trusted: Vec<String>,
}

impl CrossOrigin {
    pub fn check(&self, req: &Request) -> Result<(), &'static str> {
        if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
            return Ok(());
        }
        let header = |name: &str| {
            req.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
        };
        let origin = header("origin");
        let trusted = !origin.is_empty() && self.trusted.iter().any(|t| t == origin);
        match header("sec-fetch-site") {
            "" => {}
            "same-origin" | "none" => return Ok(()),
            _ if trusted => return Ok(()),
            _ => return Err("cross-origin request detected from Sec-Fetch-Site header"),
        }
        if origin.is_empty() {
            return Ok(());
        }
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let origin_host = url::Url::parse(origin).ok().map(|u| match u.port() {
            Some(port) => format!("{}:{port}", u.host_str().unwrap_or("")),
            None => u.host_str().unwrap_or("").to_string(),
        });
        if origin_host.as_deref() == Some(host) || trusted {
            return Ok(());
        }
        Err(
            "cross-origin request detected, and/or browser is out of date: Sec-Fetch-Site is missing, and Origin does not match Host",
        )
    }

    /// Middleware form, for `axum::middleware::from_fn_with_state`.
    pub async fn layer(
        axum::extract::State(this): axum::extract::State<CrossOrigin>,
        req: Request,
        next: Next,
    ) -> Response {
        match this.check(&req) {
            Ok(()) => next.run(req).await,
            Err(msg) => text(StatusCode::FORBIDDEN, msg),
        }
    }
}
