//! An OpenID Connect login in front of the app. Who may log in is left to the
//! provider: any valid login for the configured client gets in.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use openidconnect::core::{
    CoreAuthDisplay, CoreAuthenticationFlow, CoreClaimName, CoreClaimType, CoreClient,
    CoreClientAuthMethod, CoreGrantType, CoreJsonWebKey, CoreJweContentEncryptionAlgorithm,
    CoreJweKeyManagementAlgorithm, CoreResponseMode, CoreResponseType, CoreSubjectIdentifierType,
};
use openidconnect::{
    AdditionalProviderMetadata, AuthorizationCode, ClientId, ClientSecret, CsrfToken,
    EndpointMaybeSet, EndpointNotSet, EndpointSet, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, ProviderMetadata, RedirectUrl, Scope, TokenResponse,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::clock::{self, Clock};
use crate::db::Db;
use crate::web::{CrossOrigin, json_reply, text};

// Environment variables the app is configured with.
pub const ENV_ISSUER: &str = "OIDC_ISSUER";
pub const ENV_CLIENT_ID: &str = "OIDC_CLIENT_ID";
pub const ENV_CLIENT_SECRET: &str = "OIDC_CLIENT_SECRET";
pub const ENV_PUBLIC_URL: &str = "PUBLIC_URL";
pub const ENV_DISABLED: &str = "AUTH_DISABLED";

/// Where the provider sends the browser back. The redirect URI registered with
/// the provider is PUBLIC_URL with this on the end.
pub const CALLBACK_PATH: &str = "/auth/callback";

/// How stale a session's expiry may get before a request extends it, so a
/// page in use doesn't write on every request.
const EXTEND_EVERY: i64 = 3600;
/// How long a started login may take to finish.
const LOGIN_WINDOW: i64 = 15 * 60;
/// The session cookie. It holds only the session id.
const SESSION_COOKIE: &str = "bambu_session";
/// Carries the state of a login part-way through, so the callback can tell it
/// is finishing the login this browser started.
const LOGIN_COOKIE: &str = "bambu_login";
/// Bounds how many started logins are held at once. Starting one needs no
/// login, so without a bound anyone could grow the database. The oldest go
/// first.
pub const MAX_PENDING_LOGINS: i64 = 10_000;

/// The OIDC provider. Every field is required.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// Where the app is reached from a browser: configured rather than taken
    /// from request headers, which the client controls.
    pub public_url: String,
}

/// What the environment says to do about authentication.
#[derive(Debug, PartialEq)]
pub enum Decision {
    Required(Config),
    /// Expressly told to run without a login.
    Disabled,
}

/// Reads the environment and works out whether to require a login. There is
/// no default: a full provider configuration or AUTH_DISABLED=true is needed,
/// and anything else is an error.
pub fn decide(env: impl Fn(&str) -> Option<String>) -> Result<Decision, String> {
    let get = |name| env(name).map(|v| v.trim().to_string()).unwrap_or_default();
    let cfg = Config {
        issuer: get(ENV_ISSUER),
        client_id: get(ENV_CLIENT_ID),
        client_secret: get(ENV_CLIENT_SECRET),
        public_url: get(ENV_PUBLIC_URL),
    };
    let mut missing: Vec<&str> = [
        (ENV_ISSUER, &cfg.issuer),
        (ENV_CLIENT_ID, &cfg.client_id),
        (ENV_CLIENT_SECRET, &cfg.client_secret),
        (ENV_PUBLIC_URL, &cfg.public_url),
    ]
    .into_iter()
    .filter(|(_, v)| v.is_empty())
    .map(|(name, _)| name)
    .collect();
    missing.sort();
    let disabled = get(ENV_DISABLED) == "true";
    let configured = missing.is_empty();
    let partly = !configured && missing.len() < 4;

    if disabled && (configured || partly) {
        // Contradictory; don't let a leftover AUTH_DISABLED silently win.
        return Err(format!(
            "{ENV_DISABLED} is set to true but a provider is configured too; unset one of them to say which you meant"
        ));
    }
    if disabled {
        return Ok(Decision::Disabled);
    }
    if configured {
        let public_url = check_public_url(&cfg.public_url)?;
        return Ok(Decision::Required(Config { public_url, ..cfg }));
    }
    if partly {
        let verb = if missing.len() == 1 { "is" } else { "are" };
        return Err(format!(
            "the login provider is half configured: {} {verb} missing. Set them, or set {ENV_DISABLED}=true to run with no login at all",
            missing.join(", ")
        ));
    }
    Err(format!(
        "nothing was said about authentication. Set {ENV_ISSUER}, {ENV_CLIENT_ID}, {ENV_CLIENT_SECRET} and {ENV_PUBLIC_URL} to require a login, or {ENV_DISABLED}=true to run with none"
    ))
}

/// Checks PUBLIC_URL is a bare scheme and host, and returns it without a
/// trailing slash.
fn check_public_url(raw: &str) -> Result<String, String> {
    let u =
        url::Url::parse(raw).map_err(|e| format!("{ENV_PUBLIC_URL}={raw:?} is not a URL: {e}"))?;
    if u.scheme() != "http" && u.scheme() != "https" {
        return Err(format!(
            "{ENV_PUBLIC_URL}={raw:?} needs to start with https:// (or http://)"
        ));
    }
    let Some(host) = u.host_str() else {
        return Err(format!("{ENV_PUBLIC_URL}={raw:?} has no host"));
    };
    if !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
        || raw.contains('?')
    {
        return Err(format!(
            "{ENV_PUBLIC_URL}={raw:?} should be just the scheme and host, like https://printer.example.com"
        ));
    }
    if u.path() != "/" && !u.path().is_empty() {
        return Err(format!(
            "{ENV_PUBLIC_URL}={raw:?} has a path; the app has to be served from the root of its host"
        ));
    }
    Ok(match u.port() {
        Some(port) => format!("{}://{host}:{port}", u.scheme()),
        None => format!("{}://{host}", u.scheme()),
    })
}

// Sessions and logins part-way through.

/// One logged-in browser.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: String,
    pub subject: String,
    pub name: String,
    pub expires: i64,
}

/// A login started but not yet come back.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub verifier: String,
    pub nonce: String,
    pub next: String,
}

/// Who is logged in, and the logins part-way through. Times in unix seconds.
#[derive(Clone)]
pub struct Store {
    db: Db,
}

impl Store {
    pub fn new(db: Db) -> Store {
        Store { db }
    }

    /// Opens a session and returns the value the cookie carries.
    pub fn create_session(
        &self,
        subject: &str,
        name: &str,
        now: i64,
        expires: i64,
    ) -> rusqlite::Result<String> {
        let id = crate::random_token();
        self.db.lock().execute(
            "INSERT INTO sessions (id, subject, name, created, expires) VALUES (?, ?, ?, ?, ?)",
            params![id, subject, name, now, expires],
        )?;
        Ok(id)
    }

    /// The live session a cookie names. A lapsed row is left for the sweep,
    /// so a read stays a read.
    pub fn session(&self, id: &str, now: i64) -> rusqlite::Result<Option<Session>> {
        let found = self
            .db
            .lock()
            .query_row(
                "SELECT id, subject, name, expires FROM sessions WHERE id = ?",
                [id],
                |r| {
                    Ok(Session {
                        id: r.get(0)?,
                        subject: r.get(1)?,
                        name: r.get(2)?,
                        expires: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(found.filter(|s| s.expires > now))
    }

    /// Pushes a session's lapse out, so a phone in daily use isn't asked to log
    /// in again on a schedule.
    pub fn extend(&self, id: &str, expires: i64) -> rusqlite::Result<()> {
        self.db.lock().execute(
            "UPDATE sessions SET expires = ? WHERE id = ?",
            params![expires, id],
        )?;
        Ok(())
    }

    pub fn end_session(&self, id: &str) -> rusqlite::Result<()> {
        self.db
            .lock()
            .execute("DELETE FROM sessions WHERE id = ?", [id])?;
        Ok(())
    }

    /// Records what the callback will need to check, dropping lapsed rows and
    /// anything beyond MAX_PENDING_LOGINS (oldest by rowid, since expiries can
    /// tie).
    pub fn start_login(&self, state: &str, pending: &Pending, now: i64) -> rusqlite::Result<()> {
        let conn = self.db.lock();
        conn.execute("DELETE FROM pending_logins WHERE expires <= ?", [now])?;
        conn.execute(
            "DELETE FROM pending_logins WHERE rowid <= (
               SELECT rowid FROM pending_logins ORDER BY rowid DESC LIMIT 1 OFFSET ?)",
            [MAX_PENDING_LOGINS - 1],
        )?;
        conn.execute(
            "INSERT INTO pending_logins (state, verifier, nonce, next, expires) VALUES (?, ?, ?, ?, ?)",
            params![state, pending.verifier, pending.nonce, pending.next, now + LOGIN_WINDOW],
        )?;
        Ok(())
    }

    /// Returns and deletes what was stored for `state`, so a callback can't be
    /// replayed. Expired entries are deleted and refused.
    pub fn take_login(&self, state: &str, now: i64) -> rusqlite::Result<Option<Pending>> {
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;
        let found = tx
            .query_row(
                "SELECT verifier, nonce, next, expires FROM pending_logins WHERE state = ?",
                [state],
                |r| {
                    Ok((
                        Pending {
                            verifier: r.get(0)?,
                            nonce: r.get(1)?,
                            next: r.get(2)?,
                        },
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        tx.execute("DELETE FROM pending_logins WHERE state = ?", [state])?;
        tx.commit()?;
        Ok(found.filter(|(_, expires)| *expires >= now).map(|(p, _)| p))
    }

    /// Removes sessions and part-way logins that have lapsed.
    pub fn sweep(&self, now: i64) -> rusqlite::Result<()> {
        let conn = self.db.lock();
        conn.execute("DELETE FROM sessions WHERE expires <= ?", [now])?;
        conn.execute("DELETE FROM pending_logins WHERE expires <= ?", [now])?;
        Ok(())
    }

    /// Clears out lapsed sessions and logins every hour, forever. Spawn once.
    pub async fn run_sweeper(self, clock: Clock) {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            if let Err(err) = self.sweep(clock::secs(clock())) {
                tracing::warn!("auth: sweeping lapsed sessions: {err}");
            }
        }
    }
}

// The login itself.

/// The one piece of the provider's discovery document the standard set lacks.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct EndSession {
    end_session_endpoint: Option<String>,
}

impl AdditionalProviderMetadata for EndSession {}

type Metadata = ProviderMetadata<
    EndSession,
    CoreAuthDisplay,
    CoreClientAuthMethod,
    CoreClaimName,
    CoreClaimType,
    CoreGrantType,
    CoreJweContentEncryptionAlgorithm,
    CoreJweKeyManagementAlgorithm,
    CoreJsonWebKey,
    CoreResponseMode,
    CoreResponseType,
    CoreSubjectIdentifierType,
>;

type Client = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// Answers whether a request may proceed, and runs the login.
#[derive(Clone)]
pub struct Authenticator {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    client: Client,
    http: reqwest::Client,
    /// PUBLIC_URL, parsed: logins run on its host, and its scheme decides
    /// whether cookies are Secure.
    public: url::Url,
    /// The provider's logout URL, if it advertises one.
    end_session: Option<String>,
    session_length: Arc<dyn Fn() -> i64 + Send + Sync>,
    clock: Clock,
}

/// Served without a login: the health check and the files a phone fetches
/// before it can log in.
const OPEN_PATHS: &[&str] = &[
    "/healthz",
    "/sw.js",
    "/manifest.webmanifest",
    "/icon-192.png",
    "/icon-512.png",
];

impl Authenticator {
    /// Reads the provider's discovery document, so a wrong issuer fails at
    /// startup rather than at the first login.
    pub async fn new(
        cfg: &Config,
        store: Store,
        session_length: impl Fn() -> i64 + Send + Sync + 'static,
        clock: Clock,
    ) -> Result<Authenticator, String> {
        let public = url::Url::parse(&cfg.public_url)
            .ok()
            .filter(|u| u.host_str().is_some())
            .ok_or_else(|| {
                format!(
                    "{ENV_PUBLIC_URL}={:?} is not a URL with a host",
                    cfg.public_url
                )
            })?;
        let http = reqwest::Client::builder()
            // Following redirects here would let the provider send the token
            // request somewhere else.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        let issuer =
            IssuerUrl::new(cfg.issuer.clone()).map_err(|e| format!("{ENV_ISSUER}: {e}"))?;
        let metadata = Metadata::discover_async(issuer, &http).await.map_err(|e| {
            format!(
                "read the provider's configuration at {}: {}",
                cfg.issuer,
                error_chain(&e)
            )
        })?;
        let end_session = metadata.additional_metadata().end_session_endpoint.clone();
        let redirect = RedirectUrl::new(format!("{}{CALLBACK_PATH}", cfg.public_url))
            .map_err(|e| e.to_string())?;
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(cfg.client_id.clone()),
            Some(ClientSecret::new(cfg.client_secret.clone())),
        )
        .set_redirect_uri(redirect);
        Ok(Authenticator {
            inner: Arc::new(Inner {
                store,
                client,
                http,
                public,
                end_session,
                session_length: Arc::new(session_length),
                clock,
            }),
        })
    }

    /// The app with the login in front of it.
    pub fn wrap(&self, app: Router) -> Router {
        Router::new()
            .route("/auth/login", get(login))
            .route(CALLBACK_PATH, get(callback))
            .route("/auth/logout", post(logout))
            .route("/auth/me", get(me))
            .with_state(self.clone())
            .merge(app.layer(axum::middleware::from_fn_with_state(self.clone(), guard)))
    }

    /// The origin logins run on, for the cross-origin check.
    pub fn origin(&self) -> String {
        self.inner.public.origin().ascii_serialization()
    }

    fn now(&self) -> i64 {
        clock::secs((self.inner.clock)())
    }

    fn secure(&self) -> bool {
        self.inner.public.scheme() == "https"
    }

    /// The session cookie, or the one that takes it back (an expiry in the
    /// past).
    fn session_cookie(&self, value: &str, expires: i64) -> HeaderValue {
        cookie(SESSION_COOKIE, value, "/", expires, self.secure())
    }

    /// The live session a request carries.
    fn session(&self, headers: &HeaderMap) -> Option<Session> {
        let id = read_cookie(headers, SESSION_COOKIE)?;
        self.inner
            .store
            .session(&id, self.now())
            .unwrap_or_else(|err| {
                tracing::warn!("auth: reading a session: {err}");
                None
            })
    }
}

fn error_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        out.push_str(": ");
        out.push_str(&e.to_string());
        source = e.source();
    }
    out
}

/// A Set-Cookie value, the way Go's net/http writes one.
fn cookie(name: &str, value: &str, path: &str, expires: i64, secure: bool) -> HeaderValue {
    let when = chrono::DateTime::from_timestamp(expires.max(1), 0).unwrap_or_default();
    let mut c = format!(
        "{name}={value}; Path={path}; Expires={}; HttpOnly",
        when.format("%a, %d %b %Y %H:%M:%S GMT")
    );
    if secure {
        c.push_str("; Secure");
    }
    c.push_str("; SameSite=Lax");
    HeaderValue::from_str(&c).expect("cookie is ASCII")
}

fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn redirect(to: &str) -> Response {
    (StatusCode::FOUND, [(header::LOCATION, to.to_string())]).into_response()
}

/// Lets a request through only when it carries a live session. Pages without
/// one are sent to log in; API and camera requests get a 401.
async fn guard(State(auth): State<Authenticator>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if OPEN_PATHS.contains(&path) {
        return next.run(req).await;
    }
    let Some(session) = auth.session(req.headers()) else {
        if path.starts_with("/api/") || path.starts_with("/camera/") {
            return text(StatusCode::UNAUTHORIZED, "not logged in");
        }
        let uri = req.uri().path_and_query().map_or("/", |p| p.as_str());
        let mut to = "/auth/login".to_string();
        if uri != "/" {
            to.push_str("?next=");
            to.extend(url::form_urlencoded::byte_serialize(uri.as_bytes()));
        }
        return redirect(&to);
    };
    // Sliding expiry: extend the session and re-issue the cookie (which the
    // browser would otherwise drop at its old expiry), at most once an hour.
    let until = auth.now() + (auth.inner.session_length)();
    let mut resp = next.run(req).await;
    if session.expires < until - EXTEND_EVERY {
        match auth.inner.store.extend(&session.id, until) {
            Ok(()) => {
                resp.headers_mut()
                    .append(header::SET_COOKIE, auth.session_cookie(&session.id, until));
            }
            Err(err) => tracing::warn!("auth: extending a session: {err}"),
        }
    }
    resp
}

type Query = axum::extract::Query<std::collections::HashMap<String, String>>;

async fn login(
    State(auth): State<Authenticator>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    query: Query,
) -> Response {
    // The callback lands on PUBLIC_URL, so the state cookie must be set on that
    // host. Move a login started on another address (e.g. a LAN IP) there.
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let public_host = match auth.inner.public.port() {
        Some(port) => format!("{}:{port}", auth.inner.public.host_str().unwrap_or("")),
        None => auth.inner.public.host_str().unwrap_or("").to_string(),
    };
    if !host.eq_ignore_ascii_case(&public_host) {
        let path = uri.path_and_query().map_or("/", |p| p.as_str());
        return redirect(&format!("{}{path}", auth.origin()));
    }
    let state = crate::random_token();
    let nonce = crate::random_token();
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let pending = Pending {
        verifier: verifier.secret().clone(),
        nonce: nonce.clone(),
        next: safe_next(query.get("next").map(String::as_str).unwrap_or("")),
    };
    if let Err(err) = auth.inner.store.start_login(&state, &pending, auth.now()) {
        tracing::warn!("auth: starting a login: {err}");
        return text(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not start the login",
        );
    }
    let (to, state, _) = auth
        .inner
        .client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            move || CsrfToken::new(state),
            move || Nonce::new(nonce),
        )
        .add_scope(Scope::new("profile".into()))
        .add_scope(Scope::new("email".into()))
        .set_pkce_challenge(challenge)
        .url();
    let mut resp = redirect(to.as_str());
    // Bind the login to this browser, so an attacker can't start a login and
    // have a victim complete it.
    resp.headers_mut().append(
        header::SET_COOKIE,
        cookie(
            LOGIN_COOKIE,
            state.secret(),
            "/auth",
            auth.now() + LOGIN_WINDOW,
            auth.secure(),
        ),
    );
    resp
}

/// Keeps the post-login redirect inside this app. Browsers treat a backslash
/// as a slash, so "/\evil.example" means "//evil.example": reject backslashes,
/// a leading "//", and anything that would leave this host.
fn safe_next(next: &str) -> String {
    const HOME: &str = "/";
    if !next.starts_with('/') || next.starts_with("//") || next.contains('\\') {
        return HOME.into();
    }
    let base = url::Url::parse("http://app.invalid/").expect("base");
    let Ok(parsed) = base.join(next) else {
        return HOME.into();
    };
    if parsed.host_str() != Some("app.invalid") {
        return HOME.into();
    }
    match parsed.query() {
        Some(q) => format!("{}?{q}", parsed.path()),
        None => parsed.path().into(),
    }
}

async fn callback(State(auth): State<Authenticator>, headers: HeaderMap, query: Query) -> Response {
    let arg = |k: &str| query.get(k).cloned().unwrap_or_default();
    let reason = arg("error");
    if !reason.is_empty() {
        // The provider refused; don't loop back to it.
        return text(
            StatusCode::FORBIDDEN,
            format!("the login provider refused: {reason}"),
        );
    }
    let state = arg("state");
    // Must be the browser that started this login.
    if read_cookie(&headers, LOGIN_COOKIE)
        .is_none_or(|started| started.is_empty() || started != state)
    {
        return text(
            StatusCode::BAD_REQUEST,
            "that login was not started here, start again",
        );
    }
    // Single use.
    let clear = cookie(LOGIN_COOKIE, "", "/auth", 1, auth.secure());
    let with_clear = |mut resp: Response| {
        resp.headers_mut().append(header::SET_COOKIE, clear.clone());
        resp
    };

    let pending = match auth.inner.store.take_login(&state, auth.now()) {
        Ok(Some(p)) => p,
        _ => {
            return with_clear(text(
                StatusCode::BAD_REQUEST,
                "that login has expired, start again",
            ));
        }
    };
    let exchange = match auth
        .inner
        .client
        .exchange_code(AuthorizationCode::new(arg("code")))
    {
        Ok(request) => {
            request
                .set_pkce_verifier(PkceCodeVerifier::new(pending.verifier.clone()))
                .request_async(&auth.inner.http)
                .await
        }
        Err(err) => {
            tracing::warn!("auth: the provider has no token endpoint: {err}");
            return with_clear(text(
                StatusCode::BAD_GATEWAY,
                "the login could not be completed",
            ));
        }
    };
    let tokens = match exchange {
        Ok(tokens) => tokens,
        Err(err) => {
            tracing::warn!("auth: exchanging the code: {}", error_chain(&err));
            return with_clear(text(
                StatusCode::BAD_GATEWAY,
                "the login could not be completed",
            ));
        }
    };
    let Some(id_token) = tokens.id_token() else {
        return with_clear(text(
            StatusCode::BAD_GATEWAY,
            "the provider returned no identity token",
        ));
    };
    // Checks the signature, audience and expiry. The nonce is compared
    // separately below, so a mismatch can be told apart.
    let any_nonce = |_: Option<&Nonce>| Ok(());
    let claims = match id_token.claims(&auth.inner.client.id_token_verifier(), any_nonce) {
        Ok(claims) => claims,
        Err(err) => {
            tracing::warn!("auth: verifying the identity token: {err}");
            return with_clear(text(
                StatusCode::FORBIDDEN,
                "the identity token could not be verified",
            ));
        }
    };
    if claims.nonce().map(|n| n.secret().as_str()) != Some(pending.nonce.as_str()) {
        return with_clear(text(
            StatusCode::FORBIDDEN,
            "the identity token belongs to a different login",
        ));
    }

    let subject = claims.subject().to_string();
    let name = [
        claims
            .name()
            .and_then(|n| n.get(None))
            .map(|n| n.to_string()),
        claims.preferred_username().map(|n| n.to_string()),
        claims.email().map(|e| e.to_string()),
    ]
    .into_iter()
    .flatten()
    .find(|n| !n.is_empty())
    .unwrap_or_else(|| subject.clone());

    let now = auth.now();
    let expires = now + (auth.inner.session_length)();
    let id = match auth
        .inner
        .store
        .create_session(&subject, &name, now, expires)
    {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!("auth: opening a session: {err}");
            return with_clear(text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the login could not be completed",
            ));
        }
    };
    tracing::info!("auth: {name} logged in");
    let mut resp = with_clear(redirect(&pending.next));
    resp.headers_mut()
        .append(header::SET_COOKIE, auth.session_cookie(&id, expires));
    resp
}

/// Returns where to go rather than redirecting: the page calls this with
/// fetch, which can't follow a cross-origin redirect to the provider.
async fn logout(State(auth): State<Authenticator>, headers: HeaderMap) -> Response {
    if let Some(session) = auth.session(&headers)
        && let Err(err) = auth.inner.store.end_session(&session.id)
    {
        tracing::warn!("auth: ending a session: {err}");
    }
    let mut resp = json_reply(&json!({"then": auth.inner.end_session.as_deref().unwrap_or("/")}));
    resp.headers_mut()
        .append(header::SET_COOKIE, auth.session_cookie("", 1));
    resp
}

/// Who is logged in, so the page can show it and offer to log out.
async fn me(State(auth): State<Authenticator>, headers: HeaderMap) -> Response {
    match auth.session(&headers) {
        Some(s) => json_reply(&json!({"name": s.name, "subject": s.subject})),
        None => text(StatusCode::UNAUTHORIZED, "not logged in"),
    }
}

/// The cross-origin check, trusting PUBLIC_URL's origin when logins are on.
pub fn cross_origin(auth: Option<&Authenticator>) -> CrossOrigin {
    CrossOrigin {
        trusted: auth.map(|a| vec![a.origin()]).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests;
