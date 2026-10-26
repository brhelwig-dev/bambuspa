pub mod printer;

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_rustls::TlsAcceptor;

pub fn server_tls() -> TlsAcceptor {
    printer::acceptor()
}

pub struct FakeCamera {
    pub port: u16,
    auth: Arc<Mutex<Vec<u8>>>,
}

impl FakeCamera {
    pub async fn start(frames: Vec<Vec<u8>>) -> FakeCamera {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let auth = Arc::new(Mutex::new(Vec::new()));
        let tls = server_tls();
        let seen = auth.clone();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut conn = tls.accept(tcp).await.unwrap();
            let mut packet = vec![0u8; 80];
            conn.read_exact(&mut packet).await.unwrap();
            *seen.lock().await = packet;
            for jpeg in frames {
                let mut header = [0u8; 16];
                header[0..4].copy_from_slice(&(jpeg.len() as u32).to_le_bytes());
                conn.write_all(&header).await.unwrap();
                conn.write_all(&jpeg).await.unwrap();
            }
            conn.shutdown().await.ok();
        });
        FakeCamera { port, auth }
    }

    pub async fn auth(&self) -> Vec<u8> {
        self.auth.lock().await.clone()
    }
}

pub fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("bambuspa-test-{}", crate::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub struct Harness {
    pub app: crate::Started,
    pub printer: Option<printer::FakePrinter>,
}

pub struct Reply {
    pub status: axum::http::StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into()
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }
}

impl Harness {
    pub async fn offline() -> Harness {
        Harness::start_in(tempdir(), crate::auth::Decision::Disabled, None).await
    }

    pub async fn with_printer(state: serde_json::Value) -> Harness {
        let printer = printer::FakePrinter::start("127.0.0.1", 0, 0, 0, "SERIAL1", "secret").await;
        printer.set_state(state);
        let h = Harness::start_in(tempdir(), crate::auth::Decision::Disabled, Some(printer)).await;
        h.configure();
        h.wait(|s| s.connected && !s.fields.is_empty()).await;
        h
    }

    pub async fn start_in(
        dir: std::path::PathBuf,
        decision: crate::auth::Decision,
        printer: Option<printer::FakePrinter>,
    ) -> Harness {
        let ports = printer
            .as_ref()
            .map(|p| crate::p1s::Ports {
                mqtt: p.mqtt_port,
                camera: p.camera_port,
                ftp: p.ftp_port,
            })
            .unwrap_or_default();
        let app = crate::start(&dir, decision, ports, crate::clock::system())
            .await
            .unwrap();
        Harness { app, printer }
    }

    pub fn configure(&self) -> i64 {
        self.app
            .printers
            .add(
                "P1S",
                crate::p1s::Config {
                    ip: "127.0.0.1".into(),
                    serial: "SERIAL1".into(),
                    access_code: "secret".into(),
                },
            )
            .unwrap()
    }

    pub fn printer(&self) -> &printer::FakePrinter {
        self.printer.as_ref().expect("a printer")
    }

    pub async fn wait(&self, want: impl Fn(&crate::p1s::Snapshot) -> bool) {
        let first = self.app.printers.first().expect("a printer is set up");
        self.wait_on(first.id, want).await;
    }

    pub async fn wait_on(&self, id: i64, want: impl Fn(&crate::p1s::Snapshot) -> bool) {
        let printer = self.app.printers.get(id).expect("no such printer");
        let mut rx = printer.cache.subscribe();
        let wait = rx.wait_for(|s| want(s));
        tokio::time::timeout(std::time::Duration::from_secs(5), wait)
            .await
            .expect("timed out waiting for the printer's state")
            .unwrap();
    }

    pub async fn report(&self, fields: serde_json::Value) {
        let want = fields.clone();
        self.printer().report(fields);
        self.wait(move |s| {
            want.as_object()
                .unwrap()
                .iter()
                .all(|(k, v)| s.fields.get(k) == Some(v))
        })
        .await;
    }

    pub async fn request(&self, method: &str, uri: &str, body: &str) -> Reply {
        self.send(
            axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    pub async fn get(&self, uri: &str) -> Reply {
        self.request("GET", uri, "").await
    }

    pub async fn post(&self, uri: &str) -> Reply {
        self.request("POST", uri, "").await
    }

    pub async fn send(&self, req: axum::http::Request<axum::body::Body>) -> Reply {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let resp = self.app.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }

    pub async fn serve(&self) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = self.app.router.clone();
        tokio::spawn(async move { axum::serve(listener, router).await });
        addr
    }
}
