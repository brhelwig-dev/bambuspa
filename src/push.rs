//! Web Push notifications to subscribed browsers: RFC 8291 message encryption
//! over RFC 8188 aes128gcm, authorized by an RFC 8292 signed token.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hkdf::Hkdf;
use p256::ecdsa::{Signature, SigningKey, signature::Signer};
use p256::elliptic_curve::Generate;
use p256::elliptic_curve::sec1::ToSec1Point;
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use p256::{PublicKey, SecretKey};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use sha2::Sha256;

use crate::activity::{self, Log};
use crate::clock::{self, Clock};
use crate::db::Db;

/// How long a push service holds a message for an offline phone.
const DELIVERY_TTL: Duration = Duration::from_secs(4 * 3600);
/// How long a signed authorization token stays valid. RFC 8292 caps this at
/// 24 hours; one is minted per delivery, so it only has to outlive a request.
const TOKEN_LIFETIME: i64 = 12 * 3600;
/// Where a push service can complain about this application server, as RFC
/// 8292 requires. Never shown to the user.
pub const CONTACT: &str = "https://github.com/brhelwig-dev/bambuspa";
const RECORD_SIZE: u32 = 4096;
/// An uncompressed P-256 point: a browser's subscription key.
const KEY_LENGTH: usize = 65;

// The kinds of notification a device can ask for. Stored, so don't rename.
pub const KIND_PRINT_STARTED: &str = "print-started";
pub const KIND_PRINT_FINISHED: &str = "print-finished";
pub const KIND_PRINT_ENDED: &str = "print-ended";
pub const KIND_PRINTER_ERROR: &str = "printer-error";
pub const KIND_HEATER_OFF: &str = "heater-off";

/// Every kind a device can choose between. The bed reminder is not here: it
/// is an interval kept on the subscription, not on or off.
pub const KINDS: &[&str] = &[
    KIND_PRINT_STARTED,
    KIND_PRINT_FINISHED,
    KIND_PRINT_ENDED,
    KIND_PRINTER_ERROR,
    KIND_HEATER_OFF,
];

// Tags. A notification carrying the same tag as an earlier one replaces it on
// the phone rather than stacking beneath it.
pub const TAG_JOB: &str = "job";
pub const TAG_ERROR: &str = "error";
pub const TAG_BED: &str = "bed";
pub const TAG_NOZZLE: &str = "nozzle";

/// What shows up on the phone.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Notification {
    pub title: String,
    pub body: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tag: String,
    /// Decides which devices are told; empty means all. Not sent.
    #[serde(skip)]
    pub kind: String,
}

impl Notification {
    pub fn new(title: &str, body: &str, tag: &str, kind: &str) -> Notification {
        Notification {
            title: title.into(),
            body: body.into(),
            tag: tag.into(),
            kind: kind.into(),
        }
    }
}

// Encryption.

/// Builds one push message. The sender's key and salt are parameters so a test
/// can reproduce the RFC's worked example; production passes fresh ones.
fn encrypt(
    ua_public: &[u8],
    auth_secret: &[u8],
    plaintext: &[u8],
    as_key: &SecretKey,
    salt: &[u8; 16],
) -> Result<Vec<u8>, String> {
    let ua_key = PublicKey::from_sec1_bytes(ua_public)
        .map_err(|_| "subscription key is not a P-256 point".to_string())?;
    let shared = p256::ecdh::diffie_hellman(as_key.to_nonzero_scalar(), ua_key.as_affine());
    let as_public = as_key.public_key().to_sec1_point(false);
    let (cek, nonce) = derive_keys(
        shared.raw_secret_bytes(),
        auth_secret,
        ua_public,
        as_public.as_bytes(),
        salt,
    );

    // 0x02 marks the last record; everything sent here fits in one.
    let mut padded = plaintext.to_vec();
    padded.push(0x02);
    let sealed = Aes128Gcm::new_from_slice(&cek)
        .expect("16-byte key")
        .encrypt(&Nonce::from(nonce), padded.as_slice())
        .map_err(|_| "encryption failed".to_string())?;

    let mut out = Vec::with_capacity(16 + 4 + 1 + KEY_LENGTH + sealed.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(as_public.as_bytes().len() as u8);
    out.extend_from_slice(as_public.as_bytes());
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// RFC 8291 section 3.4.
fn derive_keys(
    shared: &[u8],
    auth_secret: &[u8],
    ua_public: &[u8],
    as_public: &[u8],
    salt: &[u8],
) -> ([u8; 16], [u8; 12]) {
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth_secret), shared)
        .expand(&key_info, &mut ikm)
        .expect("32 bytes");
    let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .expect("16 bytes");
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .expect("12 bytes");
    (cek, nonce)
}

/// Reusing a key pair or salt across messages would leak the plaintext, so
/// each message gets its own.
fn encrypt_fresh(
    ua_public: &[u8],
    auth_secret: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, String> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| e.to_string())?;
    encrypt(
        ua_public,
        auth_secret,
        plaintext,
        &SecretKey::generate(),
        &salt,
    )
}

// The application server's identity.

/// Browsers bind a subscription to the public half, so replacing this key
/// invalidates every existing subscription.
#[derive(Clone)]
pub struct Key(SecretKey);

impl Key {
    /// The key the browser needs when subscribing, base64url.
    pub fn public(&self) -> String {
        B64.encode(self.0.public_key().to_sec1_point(false).as_bytes())
    }

    /// The header proving this message came from the holder of the key the
    /// subscription was made with. The audience is the push service's origin,
    /// not the subscription path.
    fn authorization(&self, endpoint: &str, now: i64) -> Result<String, String> {
        let url =
            url::Url::parse(endpoint).map_err(|e| format!("push: endpoint {endpoint:?}: {e}"))?;
        let origin = url.origin();
        if !origin.is_tuple() {
            return Err(format!("push: endpoint {endpoint:?} has no origin"));
        }
        let claims = serde_json::json!({
            "aud": origin.ascii_serialization(),
            "exp": now + TOKEN_LIFETIME,
            "sub": CONTACT,
        });
        let signing = format!(
            "{}.{}",
            B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#),
            B64.encode(claims.to_string())
        );
        // The signature travels as r||s, not the ASN.1 structure.
        let sig: Signature = SigningKey::from(&self.0).sign(signing.as_bytes());
        Ok(format!(
            "vapid t={signing}.{}, k={}",
            B64.encode(sig.to_bytes()),
            self.public()
        ))
    }
}

// Subscriptions.

/// One browser's subscription and that device's preferences.
#[derive(Debug, Clone, PartialEq)]
pub struct Subscription {
    pub endpoint: String,
    /// The browser's public key.
    pub p256dh: Vec<u8>,
    /// The shared secret mixed into the encryption.
    pub auth: Vec<u8>,
    /// Kinds this device wants. None means it has never chosen, and gets
    /// every kind.
    pub kinds: Option<Vec<String>>,
    /// How often, in seconds, to repeat the bed reminder. Zero means never.
    pub bed_interval: i64,
}

impl Subscription {
    pub fn wants(&self, kind: &str) -> bool {
        self.kinds
            .as_ref()
            .is_none_or(|kinds| kinds.iter().any(|k| k == kind))
    }
}

/// Delivers notifications to every subscribed browser.
#[derive(Clone)]
pub struct Sender {
    db: Db,
    key: Key,
    http: reqwest::Client,
    log: Log,
    clock: Clock,
    /// Devices with a bed reminder on its way, and which printer it is about,
    /// so a reminder isn't sent twice while the first is still being delivered.
    reminding: Arc<Mutex<HashSet<(String, i64)>>>,
}

impl Sender {
    /// Loads the server's identity, creating one on first use.
    pub fn new(db: Db, log: Log, clock: Clock) -> Result<Sender, String> {
        let key = load_key(&db)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Sender {
            db,
            key,
            http,
            log,
            clock,
            reminding: Arc::default(),
        })
    }

    pub fn public_key(&self) -> String {
        self.key.public()
    }

    /// Records a browser's subscription. A browser that re-subscribes reports
    /// the same endpoint with fresh keys; its preferences are left alone, since
    /// the page re-sends its subscription on every load.
    pub fn subscribe(
        &self,
        endpoint: &str,
        p256dh: &[u8],
        auth: &[u8],
        now: i64,
    ) -> Result<(), String> {
        if endpoint.is_empty() {
            return Err("push: subscription has no endpoint".into());
        }
        if p256dh.len() != KEY_LENGTH {
            return Err(format!(
                "push: subscription key is {} bytes, want {KEY_LENGTH}",
                p256dh.len()
            ));
        }
        if auth.is_empty() {
            return Err("push: subscription has no auth secret".into());
        }
        self.db
            .lock()
            .execute(
                "INSERT INTO subscriptions (endpoint, p256dh, auth, created_ts) VALUES (?, ?, ?, ?)
                 ON CONFLICT(endpoint) DO UPDATE SET p256dh = excluded.p256dh, auth = excluded.auth",
                params![endpoint, p256dh, auth, now],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Forgets one subscription. A missing one is not an error.
    pub fn unsubscribe(&self, endpoint: &str) -> Result<(), String> {
        let conn = self.db.lock();
        conn.execute("DELETE FROM subscriptions WHERE endpoint = ?", [endpoint])
            .map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM bed_reminders WHERE endpoint = ?", [endpoint])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn count(&self) -> Result<i64, String> {
        self.db
            .lock()
            .query_row("SELECT COUNT(*) FROM subscriptions", [], |r| r.get(0))
            .map_err(|e| e.to_string())
    }

    pub fn all(&self) -> Result<Vec<Subscription>, String> {
        let conn = self.db.lock();
        let mut stmt = conn
            .prepare(
                "SELECT endpoint, p256dh, auth, kinds, bed_interval FROM subscriptions ORDER BY id",
            )
            .map_err(|e| e.to_string())?;
        stmt.query_map([], |r| {
            let kinds: Option<String> = r.get(3)?;
            Ok(Subscription {
                endpoint: r.get(0)?,
                p256dh: r.get(1)?,
                auth: r.get(2)?,
                kinds: kinds.map(|k| {
                    k.split(',')
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                }),
                bed_interval: r.get(4)?,
            })
        })
        .and_then(|rows| rows.collect())
        .map_err(|e| e.to_string())
    }

    /// One device's subscription and preferences, if it is subscribed.
    pub fn find(&self, endpoint: &str) -> Result<Option<Subscription>, String> {
        Ok(self.all()?.into_iter().find(|s| s.endpoint == endpoint))
    }

    /// Records what one device wants to be told about.
    pub fn set_preferences(
        &self,
        endpoint: &str,
        kinds: &[String],
        bed_interval: i64,
    ) -> Result<(), String> {
        if let Some(kind) = kinds.iter().find(|k| !KINDS.contains(&k.as_str())) {
            return Err(format!("push: unknown notification {kind:?}"));
        }
        if bed_interval < 0 {
            return Err("push: reminder interval cannot be negative".into());
        }
        let changed = self
            .db
            .lock()
            .execute(
                "UPDATE subscriptions SET kinds = ?, bed_interval = ? WHERE endpoint = ?",
                params![kinds.join(","), bed_interval, endpoint],
            )
            .map_err(|e| e.to_string())?;
        if changed == 0 {
            return Err("push: no such subscription".into());
        }
        Ok(())
    }

    /// Delivers to every subscription that wants `n.kind`, logs it, and
    /// reports how many were reached.
    pub async fn send(&self, n: &Notification) -> Result<usize, String> {
        let payload = serde_json::to_vec(n).map_err(|e| e.to_string())?;
        let wanted: Vec<Subscription> = self
            .all()?
            .into_iter()
            .filter(|s| n.kind.is_empty() || s.wants(&n.kind))
            .collect();
        let results = futures_util::future::join_all(
            wanted.iter().map(|s| self.deliver_or_forget(s, &payload)),
        )
        .await;
        let delivered = results.into_iter().filter(|ok| *ok).count();
        let entry = self.log.record(
            activity::NOTIFICATION,
            &format!("{} → {delivered} of {} devices", n.title, wanted.len()),
            &n.body,
        );
        let result = if delivered < wanted.len() {
            Err(format!("{} not delivered", wanted.len() - delivered))
        } else {
            Ok((self.clock)())
        };
        self.log.acknowledge(entry, result);
        Ok(delivered)
    }

    /// Delivers to one subscription and reports whether it arrived. One the
    /// push service reports gone is deleted; other failures are logged and
    /// treated as temporary.
    async fn deliver_or_forget(&self, sub: &Subscription, payload: &[u8]) -> bool {
        match self.deliver(sub, payload).await {
            Ok(true) => true,
            Ok(false) => {
                if let Err(err) = self.unsubscribe(&sub.endpoint) {
                    tracing::warn!("push: forgetting a dead subscription: {err}");
                }
                false
            }
            Err(err) => {
                tracing::warn!("push: delivery failed: {err}");
                false
            }
        }
    }

    /// Ok(false) means the push service says the subscription is gone.
    async fn deliver(&self, sub: &Subscription, payload: &[u8]) -> Result<bool, String> {
        let body = encrypt_fresh(&sub.p256dh, &sub.auth, payload)
            .map_err(|e| format!("encrypt for {}: {e}", sub.endpoint))?;
        let auth = self
            .key
            .authorization(&sub.endpoint, clock::secs((self.clock)()))?;
        let resp = self
            .http
            .post(&sub.endpoint)
            .header("Authorization", auth)
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("TTL", DELIVERY_TTL.as_secs().to_string())
            .body(body)
            .send()
            .await
            .map_err(|e| format!("post to {}: {e}", sub.endpoint))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
            return Ok(false);
        }
        if !status.is_success() {
            // Push services explain a rejection in the body.
            let detail = resp.text().await.unwrap_or_default();
            let detail: String = detail.chars().take(512).collect();
            return Err(format!("push service returned {status}: {}", detail.trim()));
        }
        Ok(true)
    }

    /// Reminds each device that printer `printer`'s bed is still on, at the
    /// interval that device chose, starting one interval after `since`. The
    /// title starts with `label` ("" or "Name: "). Returns when the next
    /// reminder comes due, so the caller can wake for it.
    pub fn remind_bed_on(
        &self,
        printer: i64,
        label: &str,
        since: i64,
        target: f64,
        now: i64,
    ) -> Option<i64> {
        let subs = match self.all().and_then(|subs| {
            let reminded = self.bed_reminded(printer)?;
            Ok(subs
                .into_iter()
                .map(|s| {
                    let at = reminded.get(&s.endpoint).copied();
                    (s, at)
                })
                .collect::<Vec<_>>())
        }) {
            Ok(subs) => subs,
            Err(err) => {
                tracing::warn!("push: bed reminders: {err}");
                return None;
            }
        };
        let mut next: Option<i64> = None;
        for (sub, reminded) in subs.into_iter().filter(|(s, _)| s.bed_interval > 0) {
            let due = reminded.unwrap_or(since) + sub.bed_interval;
            if now < due {
                next = Some(next.map_or(due, |n| n.min(due)));
                continue;
            }
            let key = (sub.endpoint.clone(), printer);
            if !self
                .reminding
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(key.clone())
            {
                continue; // already on its way
            }
            let n = Notification::new(
                &format!("{label}Bed on for {}", rounded_hours(now - since)),
                &format!("Holding {target:.0}°C."),
                &format!("{TAG_BED}-{printer}"),
                "",
            );
            let sender = self.clone();
            tokio::spawn(async move {
                let payload = serde_json::to_vec(&n).unwrap_or_default();
                if sender.deliver_or_forget(&sub, &payload).await {
                    let marked = sender.db.lock().execute(
                        "INSERT INTO bed_reminders (endpoint, printer_id, at) VALUES (?, ?, ?)
                         ON CONFLICT(endpoint, printer_id) DO UPDATE SET at = excluded.at",
                        params![sub.endpoint, printer, now],
                    );
                    if let Err(err) = marked {
                        tracing::warn!("push: recording a reminder: {err}");
                    }
                }
                sender
                    .reminding
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&key);
            });
        }
        next
    }

    /// When each device was last reminded about `printer`'s bed.
    fn bed_reminded(&self, printer: i64) -> Result<HashMap<String, i64>, String> {
        let conn = self.db.lock();
        let mut stmt = conn
            .prepare("SELECT endpoint, at FROM bed_reminders WHERE printer_id = ?")
            .map_err(|e| e.to_string())?;
        stmt.query_map([printer], |r| Ok((r.get(0)?, r.get(1)?)))
            .and_then(|rows| rows.collect())
            .map_err(|e| e.to_string())
    }

    /// Starts every device's reminder schedule for `printer` over, for when
    /// its bed goes off.
    pub fn forget_bed_reminders(&self, printer: i64) -> Result<(), String> {
        self.db
            .lock()
            .execute("DELETE FROM bed_reminders WHERE printer_id = ?", [printer])
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Seconds in whole hours, never less than one.
fn rounded_hours(secs: i64) -> String {
    let h = (secs + 1800) / 3600;
    if h <= 1 {
        "1 hour".into()
    } else {
        format!("{h} hours")
    }
}

/// The server's VAPID key, generated and stored the first time. Subscriptions
/// are bound to it, so changing it breaks every one.
fn load_key(db: &Db) -> Result<Key, String> {
    let conn = db.lock();
    let read = |conn: &rusqlite::Connection| {
        conn.query_row("SELECT der FROM server_key WHERE id = 1", [], |r| {
            r.get::<_, Vec<u8>>(0)
        })
        .optional()
    };
    let der = match read(&conn).map_err(|e| e.to_string())? {
        Some(der) => der,
        None => {
            let key = SecretKey::generate();
            let der = key.to_pkcs8_der().map_err(|e| e.to_string())?;
            conn.execute(
                "INSERT OR IGNORE INTO server_key (id, der) VALUES (1, ?)",
                [der.as_bytes()],
            )
            .map_err(|e| e.to_string())?;
            read(&conn)
                .map_err(|e| e.to_string())?
                .ok_or("push: key vanished")?
        }
    };
    SecretKey::from_pkcs8_der(&der)
        .map(Key)
        .map_err(|_| "push: stored key is not a P-256 key".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> Vec<u8> {
        B64.decode(s).unwrap()
    }

    // RFC 8291 section 5: both key pairs and the salt are fixed there, so the
    // whole message is reproducible byte for byte.
    const PLAINTEXT: &str = "When I grow up, I want to be a watermelon";
    const UA_PUBLIC: &str =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const AUTH_SECRET: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    const AS_PRIVATE: &str = "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw";
    const AS_PUBLIC: &str =
        "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";
    const SALT: &str = "DGv6ra1nlYgDCS1FRnbzlw";
    const CEK: &str = "oIhVW04MRdy2XN9CiKLxTg";
    const NONCE: &str = "4h_95klXJ5E_qnoN";
    const BODY: &str = concat!(
        "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml",
        "mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT",
        "pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
    );

    fn rfc_key() -> SecretKey {
        let key = SecretKey::from_slice(&b64(AS_PRIVATE)).unwrap();
        assert_eq!(
            B64.encode(key.public_key().to_sec1_point(false).as_bytes()),
            AS_PUBLIC
        );
        key
    }

    #[test]
    fn encrypt_matches_the_rfc_example() {
        let salt: [u8; 16] = b64(SALT).try_into().unwrap();
        let got = encrypt(
            &b64(UA_PUBLIC),
            &b64(AUTH_SECRET),
            PLAINTEXT.as_bytes(),
            &rfc_key(),
            &salt,
        )
        .unwrap();
        assert_eq!(B64.encode(got), BODY);
    }

    #[test]
    fn derive_keys_matches_the_rfc_intermediates() {
        let ua = PublicKey::from_sec1_bytes(&b64(UA_PUBLIC)).unwrap();
        let key = rfc_key();
        let shared = p256::ecdh::diffie_hellman(key.to_nonzero_scalar(), ua.as_affine());
        let (cek, nonce) = derive_keys(
            shared.raw_secret_bytes(),
            &b64(AUTH_SECRET),
            &b64(UA_PUBLIC),
            &b64(AS_PUBLIC),
            &b64(SALT),
        );
        assert_eq!(B64.encode(cek), CEK);
        assert_eq!(B64.encode(nonce), NONCE);
    }

    #[test]
    fn each_message_gets_a_fresh_key_and_salt() {
        let a = encrypt_fresh(&b64(UA_PUBLIC), &b64(AUTH_SECRET), b"x").unwrap();
        let b = encrypt_fresh(&b64(UA_PUBLIC), &b64(AUTH_SECRET), b"x").unwrap();
        assert_ne!(a[..86], b[..86]);
        assert_eq!(a[20], 65);
        assert!(encrypt_fresh(&[4; 65], &b64(AUTH_SECRET), b"x").is_err());
    }

    fn sender(db: &Db) -> Sender {
        let log = Log::new(db.clone(), || 1 << 20, clock::system()).unwrap();
        Sender::new(db.clone(), log, clock::system()).unwrap()
    }

    #[test]
    fn the_key_is_generated_once_and_kept() {
        let db = Db::memory();
        let first = sender(&db).public_key();
        assert_eq!(sender(&db).public_key(), first);
        assert_eq!(b64(&first).len(), 65);
        assert_eq!(b64(&first)[0], 4);
    }

    #[test]
    fn authorization_is_signed_and_names_the_push_service() {
        use p256::ecdsa::{VerifyingKey, signature::Verifier};
        let db = Db::memory();
        let key = load_key(&db).unwrap();
        let header = key
            .authorization("https://push.example.com/send/abc?x=1", 1000)
            .unwrap();
        let (t, k) = header
            .strip_prefix("vapid t=")
            .unwrap()
            .split_once(", k=")
            .unwrap();
        assert_eq!(k, key.public());
        let parts: Vec<&str> = t.split('.').collect();
        let claims: serde_json::Value = serde_json::from_slice(&b64(parts[1])).unwrap();
        assert_eq!(
            claims,
            serde_json::json!({"aud": "https://push.example.com", "exp": 1000 + TOKEN_LIFETIME, "sub": CONTACT})
        );
        let sig = Signature::from_slice(&b64(parts[2])).unwrap();
        let verifying = VerifyingKey::from_sec1_bytes(&b64(k)).unwrap();
        verifying
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .unwrap();
        assert!(key.authorization("not a url", 0).is_err());
    }

    fn subscribed(s: &Sender, endpoint: &str) {
        s.subscribe(endpoint, &b64(UA_PUBLIC), &b64(AUTH_SECRET), 1)
            .unwrap();
    }

    #[test]
    fn subscriptions_and_preferences() {
        let db = Db::memory();
        let s = sender(&db);
        subscribed(&s, "https://push.example.com/a");
        subscribed(&s, "https://push.example.com/b");
        assert_eq!(s.count().unwrap(), 2);
        let a = s.find("https://push.example.com/a").unwrap().unwrap();
        assert_eq!(a.kinds, None, "a device that never chose");
        assert!(a.wants(KIND_PRINT_STARTED));

        s.set_preferences(
            "https://push.example.com/a",
            &[KIND_PRINTER_ERROR.into()],
            3600,
        )
        .unwrap();
        subscribed(&s, "https://push.example.com/a");
        let a = s.find("https://push.example.com/a").unwrap().unwrap();
        assert_eq!(
            a.kinds,
            Some(vec![KIND_PRINTER_ERROR.to_string()]),
            "re-subscribing keeps preferences"
        );
        assert_eq!(a.bed_interval, 3600);
        assert!(!a.wants(KIND_PRINT_STARTED));

        s.set_preferences("https://push.example.com/a", &[], 0)
            .unwrap();
        let a = s.find("https://push.example.com/a").unwrap().unwrap();
        assert_eq!(
            a.kinds,
            Some(vec![]),
            "switching everything off is remembered"
        );
        assert!(!a.wants(KIND_PRINTER_ERROR));

        assert_eq!(
            s.set_preferences("https://push.example.com/a", &["nope".into()], 0)
                .unwrap_err(),
            "push: unknown notification \"nope\""
        );
        assert!(
            s.set_preferences("https://push.example.com/a", &[], -1)
                .is_err()
        );
        assert_eq!(
            s.set_preferences("https://x/none", &[], 0).unwrap_err(),
            "push: no such subscription"
        );

        s.unsubscribe("https://push.example.com/a").unwrap();
        s.unsubscribe("https://push.example.com/a").unwrap();
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn unusable_subscriptions_are_refused() {
        let s = sender(&Db::memory());
        assert!(s.subscribe("", &[4; 65], &[1], 0).is_err());
        assert!(s.subscribe("https://x/", &[4; 64], &[1], 0).is_err());
        assert!(s.subscribe("https://x/", &[4; 65], &[], 0).is_err());
    }

    #[test]
    fn hours() {
        assert_eq!(rounded_hours(0), "1 hour");
        assert_eq!(rounded_hours(5400), "2 hours");
        assert_eq!(rounded_hours(8 * 3600 + 100), "8 hours");
    }
}
