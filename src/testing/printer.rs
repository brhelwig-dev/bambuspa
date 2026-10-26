use std::collections::BTreeMap;
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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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

pub fn avi(frames: &[&[u8]], interval_us: u32) -> Vec<u8> {
    fn chunk(id: &[u8], body: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
        out
    }
    let mut avih = interval_us.to_le_bytes().to_vec();
    avih.resize(56, 0);
    let hdrl = [b"hdrl".to_vec(), chunk(b"avih", &avih)].concat();
    let mut movi = b"movi".to_vec();
    for frame in frames {
        movi.extend(chunk(b"00dc", frame));
    }
    let body = [
        b"AVI ".to_vec(),
        chunk(b"LIST", &hdrl),
        chunk(b"LIST", &movi),
    ]
    .concat();
    chunk(b"RIFF", &body)
}

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
    pub ftp_port: u16,
    pub serial: String,
    requests: Arc<Mutex<Vec<String>>>,
    files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    state: Arc<Mutex<Value>>,
    reports: broadcast::Sender<Value>,
}

impl FakePrinter {
    pub async fn start(
        host: &str,
        mqtt_port: u16,
        camera_port: u16,
        ftp_port: u16,
        serial: &str,
        access_code: &str,
    ) -> FakePrinter {
        let mqtt = TcpListener::bind((host, mqtt_port))
            .await
            .expect("bind mqtt");
        let camera = TcpListener::bind((host, camera_port))
            .await
            .expect("bind camera");
        let ftp = TcpListener::bind((host, ftp_port))
            .await
            .expect("bind ftp (on Linux, port 990 needs net.ipv4.ip_unprivileged_port_start=990)");
        let printer = FakePrinter {
            mqtt_port: mqtt.local_addr().unwrap().port(),
            camera_port: camera.local_addr().unwrap().port(),
            ftp_port: ftp.local_addr().unwrap().port(),
            serial: serial.into(),
            requests: Arc::default(),
            files: Arc::default(),
            state: Arc::new(Mutex::new(json!({"gcode_state": "IDLE"}))),
            reports: broadcast::Sender::new(64),
        };
        let tls = acceptor();
        let (p, t, code, h) = (
            printer.clone(),
            tls.clone(),
            access_code.to_string(),
            host.to_string(),
        );
        tokio::spawn(async move {
            while let Ok((tcp, _)) = ftp.accept().await {
                let (p, t, code, h) = (p.clone(), t.clone(), code.clone(), h.clone());
                tokio::spawn(async move {
                    if let Ok(conn) = t.accept(tcp).await {
                        let _ = p.serve_ftp(conn, t, &h, &code).await;
                    }
                });
            }
        });
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

    pub fn put_file(&self, path: &str, body: Vec<u8>) {
        self.files.lock().unwrap().insert(path.to_string(), body);
    }

    pub fn files(&self) -> Vec<String> {
        self.files.lock().unwrap().keys().cloned().collect()
    }

    fn listing(&self, dir: &str) -> Vec<u8> {
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        let mut lines = std::collections::BTreeSet::new();
        for (path, body) in self.files.lock().unwrap().iter() {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            lines.insert(match rest.split_once('/') {
                Some((sub, _)) => {
                    format!("drwxrwxrwx   1 root  root          0 Jan 02 03:04 {sub}\r\n")
                }
                None => format!(
                    "-rw-rw-rw-   1 root  root   {:>8} Jan 02 03:04 {rest}\r\n",
                    body.len()
                ),
            });
        }
        lines.into_iter().collect::<String>().into_bytes()
    }

    async fn serve_ftp<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        &self,
        conn: S,
        tls: TlsAcceptor,
        host: &str,
        code: &str,
    ) -> std::io::Result<()> {
        let (r, mut w) = tokio::io::split(conn);
        let mut lines = BufReader::new(r).lines();
        w.write_all(b"220 BBL-P003 FTP Server\r\n").await?;
        let (mut user, mut authed, mut passive) = (false, false, None);
        while let Some(line) = lines.next_line().await? {
            let (cmd, arg) = line.split_once(' ').unwrap_or((&line, ""));
            let reply = match cmd {
                "USER" => {
                    user = arg == "bblp";
                    "331 Password required".to_string()
                }
                "PASS" => {
                    authed = user && arg == code;
                    if authed {
                        "230 User logged in"
                    } else {
                        "530 Login incorrect"
                    }
                    .to_string()
                }
                "QUIT" => {
                    w.write_all(b"221 Goodbye\r\n").await?;
                    return Ok(());
                }
                _ if !authed => "530 Not logged in".to_string(),
                "PBSZ" | "PROT" | "TYPE" => "200 OK".to_string(),
                "PASV" => {
                    let listener = TcpListener::bind((host, 0)).await?;
                    let port = listener.local_addr()?.port();
                    passive = Some(listener);
                    format!(
                        "227 Entering Passive Mode (127,0,0,1,{},{})",
                        port >> 8,
                        port & 255
                    )
                }
                "LIST" | "RETR" => {
                    let Some(listener) = passive.take() else {
                        w.write_all(b"425 Use PASV first\r\n").await?;
                        continue;
                    };
                    let body = if cmd == "LIST" {
                        Some(self.listing(arg))
                    } else {
                        self.files.lock().unwrap().get(arg).cloned()
                    };
                    let Some(body) = body else {
                        w.write_all(b"550 No such file\r\n").await?;
                        continue;
                    };
                    w.write_all(b"150 Opening data connection\r\n").await?;
                    let (tcp, _) = listener.accept().await?;
                    let mut data = tls.accept(tcp).await?;
                    if data.get_ref().1.handshake_kind() != Some(rustls::HandshakeKind::Resumed) {
                        w.write_all(b"425 Data connection must reuse the TLS session\r\n")
                            .await?;
                        continue;
                    }
                    data.write_all(&body).await?;
                    data.shutdown().await?;
                    "226 Transfer complete".to_string()
                }
                "DELE" => match self.files.lock().unwrap().remove(arg) {
                    Some(_) => "250 Deleted".to_string(),
                    None => "550 No such file".to_string(),
                },
                _ => "502 ".to_string(),
            };
            w.write_all(format!("{reply}\r\n").as_bytes()).await?;
        }
        Ok(())
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
