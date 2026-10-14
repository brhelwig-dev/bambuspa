use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::BytesMut;
use rumqttc::mqttbytes::QoS;
use rumqttc::mqttbytes::v4::{
    ConnAck, ConnectReturnCode, Packet, PubAck, Publish, SubAck, SubscribeReasonCode,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::{CertifiedKey, SingleCertAndKey};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_rustls::TlsAcceptor;

pub const PIXEL_JPEG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06, 0x07, 0x06, 0x05, 0x08, 0x07, 0x07,
    0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D, 0x0C, 0x0B, 0x0B, 0x0C, 0x19, 0x12, 0x13, 0x0F,
    0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D, 0x1A, 0x1C, 0x1C, 0x20, 0x24, 0x2E, 0x27, 0x20, 0x22, 0x2C,
    0x23, 0x1C, 0x1C, 0x28, 0x37, 0x29, 0x2C, 0x30, 0x31, 0x34, 0x34, 0x34, 0x1F, 0x27, 0x39, 0x3D,
    0x38, 0x32, 0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01,
    0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00, 0x14, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0xFF, 0xC4, 0x00, 0x14,
    0x10, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0x2A, 0x9F, 0xFF, 0xD9,
];

pub fn acceptor() -> TlsAcceptor {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = CertificateDer::from_pem_slice(include_bytes!("testdata/printer-cert.pem")).unwrap();
    let key = PrivateKeyDer::from_pem_slice(include_bytes!("testdata/printer-key.pem")).unwrap();
    let key = provider.key_provider.load_private_key(key).unwrap();
    let certified = CertifiedKey::new(vec![cert], key);
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS12])
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(SingleCertAndKey::from(certified)));
    TlsAcceptor::from(Arc::new(config))
}

#[derive(Clone)]
pub struct FakePrinter {
    pub mqtt_port: u16,
    pub camera_port: u16,
    pub serial: String,
    requests: Arc<Mutex<Vec<String>>>,
    state: Arc<Mutex<Value>>,
    reports: broadcast::Sender<Value>,
}

impl FakePrinter {
    pub async fn start(
        host: &str,
        mqtt_port: u16,
        camera_port: u16,
        serial: &str,
        access_code: &str,
    ) -> FakePrinter {
        let mqtt = TcpListener::bind((host, mqtt_port))
            .await
            .expect("bind mqtt");
        let camera = TcpListener::bind((host, camera_port))
            .await
            .expect("bind camera");
        let printer = FakePrinter {
            mqtt_port: mqtt.local_addr().unwrap().port(),
            camera_port: camera.local_addr().unwrap().port(),
            serial: serial.into(),
            requests: Arc::default(),
            state: Arc::new(Mutex::new(json!({"gcode_state": "IDLE"}))),
            reports: broadcast::Sender::new(64),
        };
        let tls = acceptor();
        let (p, t, code) = (printer.clone(), tls.clone(), access_code.to_string());
        tokio::spawn(async move {
            while let Ok((tcp, _)) = mqtt.accept().await {
                let (p, t, code) = (p.clone(), t.clone(), code.clone());
                tokio::spawn(async move {
                    if let Ok(conn) = t.accept(tcp).await {
                        let _ = p.serve_mqtt(conn, &code).await;
                    }
                });
            }
        });
        tokio::spawn(async move {
            while let Ok((tcp, _)) = camera.accept().await {
                let t = tls.clone();
                tokio::spawn(async move {
                    if let Ok(mut conn) = t.accept(tcp).await {
                        let mut auth = [0u8; 80];
                        if conn.read_exact(&mut auth).await.is_err() {
                            return;
                        }
                        loop {
                            let mut header = [0u8; 16];
                            header[0..4].copy_from_slice(&(PIXEL_JPEG.len() as u32).to_le_bytes());
                            if conn.write_all(&header).await.is_err()
                                || conn.write_all(PIXEL_JPEG).await.is_err()
                            {
                                return;
                            }
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                });
            }
        });
        printer
    }

    pub fn set_state(&self, state: Value) {
        *self.state.lock().unwrap() = state;
    }

    pub fn report(&self, fields: Value) {
        if let (Value::Object(state), Value::Object(new)) =
            (&mut *self.state.lock().unwrap(), &fields)
        {
            for (k, v) in new {
                state.insert(k.clone(), v.clone());
            }
        }
        let _ = self.reports.send(json!({"print": fields}));
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    pub async fn wait_for(&self, want: impl Fn(&str) -> bool) -> String {
        for _ in 0..200 {
            if let Some(r) = self.requests().into_iter().find(|r| want(r)) {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "the printer never received the expected request; got {:?}",
            self.requests()
        );
    }

    async fn serve_mqtt<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        &self,
        mut conn: S,
        code: &str,
    ) -> std::io::Result<()> {
        let mut buf = BytesMut::new();
        let mut reports = self.reports.subscribe();
        let report_topic = format!("device/{}/report", self.serial);
        let request_topic = format!("device/{}/request", self.serial);
        let mut subscribed = false;
        loop {
            let packet = match Packet::read(&mut buf, 1 << 20) {
                Ok(packet) => packet,
                Err(rumqttc::mqttbytes::Error::InsufficientBytes(_)) => {
                    tokio::select! {
                        n = conn.read_buf(&mut buf) => if n? == 0 { return Ok(()) },
                        report = reports.recv(), if subscribed => {
                            if let Ok(report) = report {
                                write(&mut conn, Packet::Publish(Publish::new(&report_topic, QoS::AtMostOnce, report.to_string()))).await?;
                            }
                        }
                    }
                    continue;
                }
                Err(_) => return Ok(()),
            };
            match packet {
                Packet::Connect(connect) => {
                    let ok = connect
                        .login
                        .as_ref()
                        .is_some_and(|l| l.username == "bblp" && l.password == code);
                    let rc = if ok {
                        ConnectReturnCode::Success
                    } else {
                        ConnectReturnCode::BadUserNamePassword
                    };
                    write(&mut conn, Packet::ConnAck(ConnAck::new(rc, false))).await?;
                    if !ok {
                        return Ok(());
                    }
                }
                Packet::Subscribe(sub) => {
                    subscribed |= sub.filters.iter().any(|f| f.path == report_topic);
                    let codes = sub
                        .filters
                        .iter()
                        .map(|_| SubscribeReasonCode::Success(QoS::AtMostOnce))
                        .collect();
                    write(&mut conn, Packet::SubAck(SubAck::new(sub.pkid, codes))).await?;
                }
                Packet::Publish(msg) if msg.topic == request_topic => {
                    let payload = String::from_utf8_lossy(&msg.payload).to_string();
                    self.requests.lock().unwrap().push(payload.clone());
                    if msg.qos == QoS::AtLeastOnce {
                        write(&mut conn, Packet::PubAck(PubAck::new(msg.pkid))).await?;
                    }
                    if payload.contains("\"pushall\"") {
                        let state = json!({"print": self.state.lock().unwrap().clone()});
                        write(
                            &mut conn,
                            Packet::Publish(Publish::new(
                                &report_topic,
                                QoS::AtMostOnce,
                                state.to_string(),
                            )),
                        )
                        .await?;
                    }
                }
                Packet::PingReq => write(&mut conn, Packet::PingResp).await?,
                Packet::Disconnect => return Ok(()),
                _ => {}
            }
        }
    }
}

async fn write<S: tokio::io::AsyncWrite + Unpin>(
    conn: &mut S,
    packet: Packet,
) -> std::io::Result<()> {
    let mut out = BytesMut::new();
    packet
        .write(&mut out, 1 << 22)
        .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
    conn.write_all(&out).await?;
    conn.flush().await
}
