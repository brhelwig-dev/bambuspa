//! The environment rules, the session store, and the whole login run against
//! a stand-in provider that signs real tokens — so verification is exercised
//! the way a live provider would exercise it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use openidconnect::core::{
    CoreIdToken, CoreIdTokenClaims, CoreJsonWebKeySet, CoreJwsSigningAlgorithm,
    CoreRsaPrivateSigningKey,
};
use openidconnect::{
    Audience, EmptyAdditionalClaims, EndUserName, IssuerUrl, JsonWebKeyId, LocalizedClaim, Nonce,
    PrivateSigningKey, StandardClaims, SubjectIdentifier,
};
use serde_json::json;

use super::*;
use crate::clock;
use crate::testing::tempdir;

// The environment.

fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let vars: HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |name| vars.get(name).cloned()
}

const COMPLETE: &[(&str, &str)] = &[
    (ENV_ISSUER, "https://id.example.com"),
    (ENV_CLIENT_ID, "bambu"),
    (ENV_CLIENT_SECRET, "s3cret"),
    (ENV_PUBLIC_URL, "https://printer.example.com/"),
];

#[test]
fn a_complete_provider_requires_a_login() {
    let Decision::Required(cfg) = decide(env(COMPLETE)).unwrap() else {
        panic!("not required")
    };
    assert_eq!(cfg.issuer, "https://id.example.com");
    assert_eq!(
        cfg.public_url, "https://printer.example.com",
        "trailing slash dropped"
    );
}

#[test]
fn auth_disabled_switches_it_off_and_only_exactly_true() {
    assert_eq!(
        decide(env(&[(ENV_DISABLED, " true ")])).unwrap(),
        Decision::Disabled
    );
    for value in ["TRUE", "1", "yes"] {
        assert!(decide(env(&[(ENV_DISABLED, value)])).is_err(), "{value}");
    }
}

#[test]
fn saying_nothing_is_refused_and_names_every_option() {
    let err = decide(env(&[])).unwrap_err();
    for name in [
        ENV_ISSUER,
        ENV_CLIENT_ID,
        ENV_CLIENT_SECRET,
        ENV_PUBLIC_URL,
        ENV_DISABLED,
    ] {
        assert!(err.contains(name), "{err}");
    }
}

#[test]
fn a_half_configured_provider_names_what_is_missing() {
    for missing in [ENV_ISSUER, ENV_CLIENT_ID, ENV_CLIENT_SECRET, ENV_PUBLIC_URL] {
        let partial: Vec<(&str, &str)> = COMPLETE
            .iter()
            .copied()
            .filter(|(k, _)| *k != missing)
            .collect();
        let err = decide(env(&partial)).unwrap_err();
        assert!(
            err.contains(missing) && err.contains(" is missing"),
            "{err}"
        );
    }
    let err = decide(env(&COMPLETE[..2])).unwrap_err();
    assert!(
        err.contains(&format!(
            "{ENV_CLIENT_SECRET}, {ENV_PUBLIC_URL} are missing"
        )),
        "{err}"
    );
}

#[test]
fn both_at_once_is_refused() {
    let mut both = COMPLETE.to_vec();
    both.push((ENV_DISABLED, "true"));
    assert!(decide(env(&both)).unwrap_err().contains(ENV_DISABLED));
}

#[test]
fn a_bad_public_url_is_refused() {
    for bad in [
        "printer.example.com",
        "ftp://printer.example.com",
        "https://",
        "https://user@printer.example.com",
        "https://printer.example.com/app",
        "https://printer.example.com?x=1",
        "https://printer.example.com/?",
        "https://printer.example.com#top",
    ] {
        let mut given = COMPLETE[..3].to_vec();
        given.push((ENV_PUBLIC_URL, bad));
        let err = decide(env(&given)).unwrap_err();
        assert!(err.contains(ENV_PUBLIC_URL), "{bad}: {err}");
    }
}

#[test]
fn safe_next_keeps_logins_inside_the_app() {
    for away in [
        "https://evil.example.com/",
        "//evil.example.com/",
        r"/\evil.example.com/",
        r"/\/evil.example.com/",
        r"\\evil.example.com/",
        "http:/evil.example.com",
        "",
    ] {
        assert_eq!(safe_next(away), "/", "{away}");
    }
    for inside in ["/", "/api/status", "/camera/history/frame?ts=5"] {
        assert_eq!(safe_next(inside), inside);
    }
    assert_eq!(safe_next("/a b#frag"), "/a%20b");
}

// The store.

#[test]
fn sessions_live_until_they_lapse_and_can_be_extended_or_ended() {
    let store = Store::new(Db::memory());
    let id = store.create_session("user-1", "Ada", 100, 200).unwrap();
    assert_eq!(id.len(), 43);
    assert_eq!(store.session(&id, 150).unwrap().unwrap().name, "Ada");
    assert_eq!(store.session(&id, 200).unwrap(), None);
    store.extend(&id, 300).unwrap();
    assert!(store.session(&id, 250).unwrap().is_some());
    store.end_session(&id).unwrap();
    assert_eq!(store.session(&id, 250).unwrap(), None);
    assert_eq!(store.session("nope", 0).unwrap(), None);
}

#[test]
fn a_started_login_is_taken_once_and_lapses() {
    let store = Store::new(Db::memory());
    let pending = Pending {
        verifier: "v".into(),
        nonce: "n".into(),
        next: "/x".into(),
    };
    store.start_login("s1", &pending, 1000).unwrap();
    assert_eq!(
        store.take_login("s1", 1000 + LOGIN_WINDOW).unwrap(),
        Some(pending.clone())
    );
    assert_eq!(store.take_login("s1", 1000).unwrap(), None, "single use");
    store.start_login("s2", &pending, 1000).unwrap();
    assert_eq!(
        store.take_login("s2", 1000 + LOGIN_WINDOW + 1).unwrap(),
        None
    );
}

#[test]
fn the_sweep_clears_what_has_lapsed() {
    let db = Db::memory();
    let store = Store::new(db.clone());
    store.create_session("a", "", 0, 100).unwrap();
    let live = store.create_session("b", "", 0, 2000).unwrap();
    store
        .start_login(
            "s",
            &Pending {
                verifier: "v".into(),
                nonce: "n".into(),
                next: "/".into(),
            },
            0,
        )
        .unwrap();
    store.sweep(1000).unwrap();
    let count = |table: &str| {
        db.lock()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
    };
    assert_eq!((count("sessions"), count("pending_logins")), (1, 0));
    assert!(store.session(&live, 1000).unwrap().is_some());
}

#[test]
fn part_way_logins_are_bounded() {
    let db = Db::memory();
    let store = Store::new(db.clone());
    let p = Pending {
        verifier: "v".into(),
        nonce: "n".into(),
        next: "/".into(),
    };
    db.lock()
        .execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?)
             INSERT INTO pending_logins SELECT 'old' || i, 'v', 'n', '/', 99999 FROM n",
            [MAX_PENDING_LOGINS + 5],
        )
        .unwrap();
    store.start_login("newest", &p, 0).unwrap();
    let count: i64 = db
        .lock()
        .query_row("SELECT COUNT(*) FROM pending_logins", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, MAX_PENDING_LOGINS);
    assert!(store.take_login("newest", 0).unwrap().is_some());
    assert!(
        store.take_login("old1", 0).unwrap().is_none(),
        "the oldest went first"
    );
}

// The login, end to end.

const KEY_PEM: &str = include_str!("testdata/provider-key.pem");

#[derive(Default)]
struct Seen {
    nonce: String,
    challenge: String,
    verifier: String,
}

/// Levers a test pulls to make the provider misbehave.
#[derive(Default, Clone)]
struct Levers {
    wrong_key: bool,
    other_nonce: bool,
    name: Option<String>,
}

#[derive(Clone)]
struct Provider {
    url: String,
    seen: Arc<Mutex<Seen>>,
    levers: Arc<Mutex<Levers>>,
}

impl Provider {
    async fn start() -> Provider {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let p = Provider {
            url,
            seen: Arc::default(),
            levers: Arc::default(),
        };
        let router = axum::Router::new()
            .route(
                "/.well-known/openid-configuration",
                axum::routing::get(discovery),
            )
            .route("/jwks", axum::routing::get(jwks))
            .route("/authorize", axum::routing::get(authorize))
            .route("/token", axum::routing::post(token))
            .with_state(p.clone());
        tokio::spawn(async move { axum::serve(listener, router).await });
        p
    }
}

fn signing_key(kid: &str) -> CoreRsaPrivateSigningKey {
    CoreRsaPrivateSigningKey::from_pem(KEY_PEM, Some(JsonWebKeyId::new(kid.into()))).unwrap()
}

async fn discovery(State(p): State<Provider>) -> impl IntoResponse {
    axum::Json(json!({
        "issuer": p.url,
        "authorization_endpoint": format!("{}/authorize", p.url),
        "token_endpoint": format!("{}/token", p.url),
        "jwks_uri": format!("{}/jwks", p.url),
        "end_session_endpoint": format!("{}/logout", p.url),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
    }))
}

async fn jwks() -> impl IntoResponse {
    axum::Json(CoreJsonWebKeySet::new(vec![
        signing_key("k1").as_verification_key(),
    ]))
}

async fn authorize(
    State(p): State<Provider>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let mut seen = p.seen.lock().unwrap();
    seen.nonce = q["nonce"].clone();
    seen.challenge = q["code_challenge"].clone();
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(q["scope"].contains("openid") && q["scope"].contains("email"));
    Redirect::to(&format!(
        "{}?code=c1&state={}",
        q["redirect_uri"], q["state"]
    ))
}

async fn token(
    State(p): State<Provider>,
    axum::Form(form): axum::Form<HashMap<String, String>>,
) -> impl IntoResponse {
    let levers = p.levers.lock().unwrap().clone();
    let nonce = {
        let mut seen = p.seen.lock().unwrap();
        seen.verifier = form.get("code_verifier").cloned().unwrap_or_default();
        if levers.other_nonce {
            "some-other-login".to_string()
        } else {
            seen.nonce.clone()
        }
    };
    let now = chrono::Utc::now();
    let mut name = LocalizedClaim::new();
    name.insert(
        None,
        EndUserName::new(levers.name.unwrap_or_else(|| "Ada".into())),
    );
    let claims = CoreIdTokenClaims::new(
        IssuerUrl::new(p.url.clone()).unwrap(),
        vec![Audience::new("bambu".into())],
        now + chrono::Duration::minutes(5),
        now,
        StandardClaims::new(SubjectIdentifier::new("user-1".into())).set_name(Some(name)),
        EmptyAdditionalClaims {},
    )
    .set_nonce(Some(Nonce::new(nonce)));
    // A key the provider never published can be told apart only by checking
    // the signature: same id, different key.
    let key = if levers.wrong_key {
        let other = rsa_other_pem();
        CoreRsaPrivateSigningKey::from_pem(&other, Some(JsonWebKeyId::new("k1".into()))).unwrap()
    } else {
        signing_key("k1")
    };
    let id_token = CoreIdToken::new(
        claims,
        &key,
        CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
        None,
        None,
    )
    .unwrap();
    axum::Json(
        json!({"access_token": "a", "token_type": "bearer", "expires_in": 3600, "id_token": id_token.to_string()}),
    )
}

fn rsa_other_pem() -> String {
    std::process::Command::new("openssl")
        .args(["genrsa", "-traditional", "2048"])
        .output()
        .map(|o| String::from_utf8(o.stdout).unwrap())
        .unwrap()
}

/// A browser: follows redirects and keeps cookies.
struct Browser {
    http: reqwest::Client,
    cookies: Mutex<HashMap<String, String>>,
    /// Every URL visited, in order.
    visited: Mutex<Vec<String>>,
}

struct Page {
    status: StatusCode,
    body: String,
    set_cookies: Vec<String>,
}

impl Browser {
    fn new() -> Browser {
        Browser {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            cookies: Mutex::default(),
            visited: Mutex::default(),
        }
    }

    async fn go(&self, method: &str, url: &str) -> Page {
        self.go_until(method, url, |_| false).await
    }

    /// Follows redirects, stopping before any URL `stop` matches.
    async fn go_until(&self, method: &str, url: &str, stop: impl Fn(&str) -> bool) -> Page {
        let mut url = url.to_string();
        let mut method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut set_cookies = Vec::new();
        for _ in 0..10 {
            self.visited.lock().unwrap().push(url.clone());
            let cookie = self
                .cookies
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            let resp = self
                .http
                .request(method.clone(), &url)
                .header("cookie", cookie)
                .send()
                .await
                .unwrap();
            for c in resp.headers().get_all("set-cookie") {
                let c = c.to_str().unwrap().to_string();
                let (pair, rest) = c.split_once(';').unwrap_or((&c, ""));
                let (k, v) = pair.split_once('=').unwrap();
                if v.is_empty() || rest.contains("1970") {
                    self.cookies.lock().unwrap().remove(k);
                } else {
                    self.cookies.lock().unwrap().insert(k.into(), v.into());
                }
                set_cookies.push(c);
            }
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap();
            if status.is_redirection() {
                let to = resp.headers()["location"].to_str().unwrap();
                let next = reqwest::Url::parse(&url)
                    .unwrap()
                    .join(to)
                    .unwrap()
                    .to_string();
                if stop(&next) {
                    self.visited.lock().unwrap().push(next);
                    return Page {
                        status,
                        body: String::new(),
                        set_cookies,
                    };
                }
                url = next;
                method = reqwest::Method::GET;
                continue;
            }
            return Page {
                status,
                body: resp.text().await.unwrap(),
                set_cookies,
            };
        }
        panic!("redirect loop");
    }
}

struct Setup {
    url: String,
    provider: Provider,
    clock: clock::Fake,
    store: Store,
}

async fn setup() -> Setup {
    let provider = Provider::start().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let cfg = Config {
        issuer: provider.url.clone(),
        client_id: "bambu".into(),
        client_secret: "s3cret".into(),
        public_url: url.clone(),
    };
    let fake = clock::Fake::at(clock::system()());
    let dir = tempdir();
    let started = crate::start(
        &dir,
        Decision::Required(cfg),
        crate::p1s::Ports::default(),
        fake.clock(),
    )
    .await
    .unwrap();
    tokio::spawn(async move { axum::serve(listener, started.router).await });
    let store = Store::new(Db::open(dir.join(crate::DB_FILE)).unwrap());
    Setup {
        url,
        provider,
        clock: fake,
        store,
    }
}

fn challenge_for(verifier: &str) -> String {
    use base64::Engine;
    use sha2::Digest;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()))
}

#[tokio::test]
async fn logging_in_and_reaching_the_app() {
    let s = setup().await;
    let b = Browser::new();
    assert_eq!(
        b.go("GET", &format!("{}/api/status", s.url)).await.status,
        StatusCode::UNAUTHORIZED
    );
    let page = b.go("GET", &format!("{}/", s.url)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("<script>"));
    assert_eq!(
        b.go("GET", &format!("{}/api/status", s.url)).await.status,
        StatusCode::OK
    );

    // PKCE really happened.
    let seen = s.provider.seen.lock().unwrap();
    assert!(!seen.verifier.is_empty());
    assert_eq!(challenge_for(&seen.verifier), seen.challenge);
}

#[tokio::test]
async fn without_a_login_the_page_redirects_and_the_api_refuses() {
    let s = setup().await;
    let b = Browser::new();
    let page = b
        .go_until("GET", &format!("{}/settings?y=1", s.url), |u| {
            u.contains("/auth/login")
        })
        .await;
    assert_eq!(page.status, StatusCode::FOUND);
    assert_eq!(
        b.visited.lock().unwrap().last().unwrap(),
        &format!("{}/auth/login?next=%2Fsettings%3Fy%3D1", s.url)
    );
    for path in ["/api/status", "/camera/history/range"] {
        let r = b.go("GET", &format!("{}{path}", s.url)).await;
        assert_eq!(
            (r.status, r.body.as_str()),
            (StatusCode::UNAUTHORIZED, "not logged in\n")
        );
    }
    for path in [
        "/healthz",
        "/sw.js",
        "/manifest.webmanifest",
        "/icon-192.png",
        "/icon-512.png",
    ] {
        assert_eq!(
            b.go("GET", &format!("{}{path}", s.url)).await.status,
            StatusCode::OK,
            "{path}"
        );
    }
}

#[tokio::test]
async fn a_login_returns_to_where_it_started_and_never_leaves_the_app() {
    let s = setup().await;
    let b = Browser::new();
    b.go("GET", &format!("{}/auth/login?next=%2Fapi%2Fstatus", s.url))
        .await;
    assert_eq!(
        b.visited.lock().unwrap().last().unwrap(),
        &format!("{}/api/status", s.url)
    );

    let b = Browser::new();
    b.go(
        "GET",
        &format!("{}/auth/login?next=%2F%5Cevil.example.com%2F", s.url),
    )
    .await;
    assert_eq!(
        b.visited.lock().unwrap().last().unwrap(),
        &format!("{}/", s.url)
    );
}

#[tokio::test]
async fn the_session_cookie_is_locked_down() {
    let s = setup().await;
    let cookies = Browser::new()
        .go("GET", &format!("{}/auth/login", s.url))
        .await
        .set_cookies;
    let session = cookies
        .iter()
        .find(|c| c.starts_with("bambu_session=") && !c.starts_with("bambu_session=;"))
        .expect("a session cookie");
    assert!(
        session.contains("; Path=/;")
            && session.contains("; HttpOnly")
            && session.ends_with("; SameSite=Lax"),
        "{session}"
    );
    assert!(!session.contains("Secure"), "not https");
    let login = cookies
        .iter()
        .find(|c| c.starts_with("bambu_login=") && c.len() > 20)
        .expect("a login cookie");
    assert!(login.contains("; Path=/auth;"), "{login}");
}

#[tokio::test]
async fn a_token_signed_with_the_wrong_key_is_refused() {
    let s = setup().await;
    s.provider.levers.lock().unwrap().wrong_key = true;
    let b = Browser::new();
    let page = b.go("GET", &format!("{}/auth/login", s.url)).await;
    assert_eq!(
        (page.status, page.body.as_str()),
        (
            StatusCode::FORBIDDEN,
            "the identity token could not be verified\n"
        )
    );
    assert_eq!(
        b.go("GET", &format!("{}/api/status", s.url)).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_token_for_a_different_login_is_refused() {
    let s = setup().await;
    s.provider.levers.lock().unwrap().other_nonce = true;
    let page = Browser::new()
        .go("GET", &format!("{}/auth/login", s.url))
        .await;
    assert_eq!(
        (page.status, page.body.as_str()),
        (
            StatusCode::FORBIDDEN,
            "the identity token belongs to a different login\n"
        )
    );
}

#[tokio::test]
async fn callbacks_that_should_not_work_do_not() {
    let s = setup().await;
    let b = Browser::new();
    let r = b
        .go(
            "GET",
            &format!("{}/auth/callback?code=x&state=never-issued", s.url),
        )
        .await;
    assert_eq!(
        (r.status, r.body.as_str()),
        (
            StatusCode::BAD_REQUEST,
            "that login was not started here, start again\n"
        )
    );
    let r = b
        .go(
            "GET",
            &format!("{}/auth/callback?error=access_denied", s.url),
        )
        .await;
    assert_eq!(
        (r.status, r.body.as_str()),
        (
            StatusCode::FORBIDDEN,
            "the login provider refused: access_denied\n"
        )
    );

    // Started by one browser, finished by another.
    let starter = Browser::new();
    starter
        .go_until("GET", &format!("{}/auth/login", s.url), |u| {
            u.contains("/auth/callback")
        })
        .await;
    let callback = starter.visited.lock().unwrap().last().unwrap().clone();
    let victim = Browser::new();
    let r = victim.go("GET", &callback).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        victim
            .go("GET", &format!("{}/api/status", s.url))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    // Replayed by the same browser after it was used.
    assert_eq!(
        starter.go("GET", &callback).await.status,
        StatusCode::OK,
        "finishing it logs in"
    );
    let again = starter.go("GET", &callback).await;
    assert_eq!(
        again.status,
        StatusCode::BAD_REQUEST,
        "a callback cannot be replayed"
    );
}

#[tokio::test]
async fn sessions_lapse_slide_and_end() {
    let s = setup().await;
    let b = Browser::new();
    b.go("GET", &format!("{}/auth/login", s.url)).await;
    let id: String = s
        .store
        .db
        .lock()
        .query_row("SELECT id FROM sessions", [], |r| r.get(0))
        .unwrap();
    let before = s.store.session(&id, 0).unwrap().unwrap().expires;

    // Within the hour: not written again.
    s.clock.advance(60);
    let r = b.go("GET", &format!("{}/api/status", s.url)).await;
    assert!(r.set_cookies.is_empty());
    assert_eq!(s.store.session(&id, 0).unwrap().unwrap().expires, before);

    // A day later: extended, and the cookie re-issued.
    s.clock.advance(24 * 3600);
    let r = b.go("GET", &format!("{}/api/status", s.url)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.set_cookies
            .iter()
            .any(|c| c.starts_with("bambu_session="))
    );
    assert!(s.store.session(&id, 0).unwrap().unwrap().expires > before);

    // A year later: lapsed.
    s.clock.advance(365 * 24 * 3600);
    assert_eq!(
        b.go("GET", &format!("{}/api/status", s.url)).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn logging_out_ends_the_session_and_says_where_to_go() {
    let s = setup().await;
    let b = Browser::new();
    b.go("GET", &format!("{}/auth/login", s.url)).await;
    let r = b.go("POST", &format!("{}/auth/logout", s.url)).await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&r.body).unwrap(),
        json!({"then": format!("{}/logout", s.provider.url)})
    );
    assert_eq!(
        b.go("GET", &format!("{}/api/status", s.url)).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        b.go("GET", &format!("{}/auth/me", s.url)).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn who_is_logged_in_is_real_json() {
    let s = setup().await;
    s.provider.levers.lock().unwrap().name = Some("Ada \"Lovelace\"\u{7f} 😀".into());
    let b = Browser::new();
    b.go("GET", &format!("{}/auth/login", s.url)).await;
    let who: serde_json::Value =
        serde_json::from_str(&b.go("GET", &format!("{}/auth/me", s.url)).await.body).unwrap();
    assert_eq!(
        who,
        json!({"name": "Ada \"Lovelace\"\u{7f} 😀", "subject": "user-1"})
    );
}

#[tokio::test]
async fn a_login_on_another_host_is_moved_to_the_public_one() {
    let s = setup().await;
    let other = s.url.replace("127.0.0.1", "localhost");
    let b = Browser::new();
    b.go_until("GET", &format!("{other}/auth/login?next=%2Fx"), |u| {
        u.contains("/authorize")
    })
    .await;
    let visited = b.visited.lock().unwrap();
    assert_eq!(visited[1], format!("{}/auth/login?next=%2Fx", s.url));
}

#[tokio::test]
async fn a_post_from_another_origin_is_refused_but_the_public_one_is_trusted() {
    let s = setup().await;
    let b = Browser::new();
    b.go("GET", &format!("{}/auth/login", s.url)).await;
    let cookie = b
        .cookies
        .lock()
        .unwrap()
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    let post = |origin: &str, site: &str| {
        b.http
            .post(format!("{}/api/settings/camera-storage?value=512", s.url))
            .header("cookie", cookie.clone())
            .header("origin", origin.to_string())
            .header("sec-fetch-site", site.to_string())
            .send()
    };
    assert_eq!(
        post("https://evil.example", "cross-site")
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(post(&s.url, "same-origin").await.unwrap().status(), 204);
    assert_eq!(
        post(&s.url, "same-site").await.unwrap().status(),
        204,
        "PUBLIC_URL's own origin is trusted"
    );
}
