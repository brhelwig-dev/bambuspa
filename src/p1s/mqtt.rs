//! The MQTT link to the printer: reports in, commands out.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rumqttc::{
    AsyncClient, ConnectReturnCode, ConnectionError, Event, EventLoop, MqttOptions, Outgoing,
    Packet, QoS, TlsConfiguration, Transport,
};
use serde_json::{Value, json};

use super::StateCache;
use crate::activity::{self, Log};
use crate::clock::Clock;

/// Z200 — 50mm above the bottom of the ~250mm travel; covers position drift
/// without homing (a blind Z250 once hit the bottom limit).
pub const BED_DROP_GCODE: &str = "G90\nG1 Z200 F900\n";
pub const HOME_GCODE: &str = "G28\n";
const EXTRUDE_GCODE: &str = "M83\nG1 E20 F150\n";

/// The full-state request sent on every (re)connect, since the printer
/// otherwise only sends changes.
const PUSHALL: &str = r#"{"pushing":{"sequence_id":"0","command":"pushall"}}"#;

/// How long to wait for the broker to confirm before recording that it didn't.
const ACK_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(15);

/// How long a connection check waits for the printer to answer at all, and
/// then for its first report.
const PROBE_CONNECT: Duration = Duration::from_secs(8);
const PROBE_REPORT: Duration = Duration::from_secs(5);

/// The connection settings for one printer.
fn options(host: &str, port: u16, access_code: &str) -> MqttOptions {
    let id = format!("bambuspa-{}", &crate::random_token()[..8]);
    let mut options = MqttOptions::new(id, host, port);
    options
        .set_credentials("bblp", access_code)
        .set_keep_alive(Duration::from_secs(30))
        // The first report after connecting is the printer's whole state.
        .set_max_packet_size(4 << 20, 1 << 20)
        .set_transport(Transport::tls_with_config(TlsConfiguration::Rustls(
            super::tls::client_config(),
        )));
    options
}

/// Says in plain words why the printer could not be reached.
pub fn describe(err: &ConnectionError, host: &str) -> String {
    use std::io::ErrorKind;
    let io = |e: &std::io::Error| match e.kind() {
        ErrorKind::ConnectionRefused => format!(
            "{host} refused the connection. Check the address, and that LAN mode is on in the printer's network settings."
        ),
        ErrorKind::TimedOut | ErrorKind::HostUnreachable | ErrorKind::NetworkUnreachable => {
            format!(
                "No answer from {host}. Check the address, and that the printer is on and on this network."
            )
        }
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::UnexpectedEof => {
            "The printer closed the connection. Check the access code.".into()
        }
        _ => format!("Could not reach {host}: {e}"),
    };
    match err {
        ConnectionError::NetworkTimeout => {
            format!(
                "No answer from {host}. Check the address, and that the printer is on and on this network."
            )
        }
        ConnectionError::Io(e) => io(e),
        ConnectionError::Tls(rumqttc::TlsError::Io(e)) => io(e),
        ConnectionError::Tls(e) => {
            format!("{host} answered, but not like a Bambu printer ({e}). Check the address.")
        }
        ConnectionError::ConnectionRefused(
            ConnectReturnCode::BadUserNamePassword | ConnectReturnCode::NotAuthorized,
        ) => "The printer rejected the access code.".into(),
        ConnectionError::ConnectionRefused(code) => {
            format!("The printer refused the connection ({code:?}).")
        }
        other => format!("Lost the connection to the printer: {other}"),
    }
}

/// Connects once with `conf` and waits for the printer's first report, so a
/// wrong address, access code or serial is caught before it is saved.
pub async fn probe(host: &str, port: u16, serial: &str, access_code: &str) -> Result<(), String> {
    let mut opts = options(host, port, access_code);
    opts.set_clean_session(true);
    let (client, mut events) = AsyncClient::new(opts, 8);
    events
        .network_options
        .set_connection_timeout(PROBE_CONNECT.as_secs());
    let report_topic = format!("device/{serial}/report");
    let result = async {
        // Connected and signed in.
        loop {
            match events.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => break,
                Ok(_) => {}
                Err(err) => return Err(describe(&err, host)),
            }
        }
        client
            .subscribe(&report_topic, QoS::AtMostOnce)
            .await
            .map_err(|e| e.to_string())?;
        client
            .publish(
                format!("device/{serial}/request"),
                QoS::AtMostOnce,
                false,
                PUSHALL,
            )
            .await
            .map_err(|e| e.to_string())?;
        let wrong_serial = || {
            format!(
                "Signed in, but the printer didn't answer for serial {serial}. Check the serial."
            )
        };
        let answer = async {
            loop {
                match events.poll().await {
                    Ok(Event::Incoming(Packet::Publish(msg))) if msg.topic == report_topic => {
                        return Ok(());
                    }
                    Ok(_) => {}
                    // A printer drops a client that asks about another serial.
                    Err(_) => return Err(wrong_serial()),
                }
            }
        };
        tokio::time::timeout(PROBE_REPORT, answer)
            .await
            .unwrap_or_else(|_| Err(wrong_serial()))
    };
    let result = tokio::time::timeout(PROBE_CONNECT + PROBE_REPORT, result)
        .await
        .unwrap_or_else(|_| {
            Err(format!("No answer from {host}. Check the address, and that the printer is on and on this network."))
        });
    let _ = client.try_disconnect();
    result
}

/// A command payload: `{namespace: {sequence_id, command, ...fields}}`, keys in
/// alphabetical order.
pub fn request(seq: i64, namespace: &str, command: &str, fields: &[(&str, Value)]) -> String {
    let mut body = BTreeMap::new();
    body.insert("sequence_id", json!(seq.to_string()));
    body.insert("command", json!(command));
    for (k, v) in fields {
        body.insert(k, v.clone());
    }
    serde_json::to_string(&BTreeMap::from([(namespace, body)])).expect("command payload")
}

/// Names a command from its payload, so the log reads as a list of what was
/// asked for rather than a wall of JSON.
pub fn summarize(payload: &str) -> String {
    let Ok(msg) = serde_json::from_str::<Value>(payload) else {
        return "command".into();
    };
    for namespace in ["print", "system", "pushing"] {
        let Some(section) = msg.get(namespace) else {
            continue;
        };
        let Some(name) = section.get("command").and_then(Value::as_str) else {
            continue;
        };
        if name == "gcode_line"
            && let Some(line) = section.get("param").and_then(Value::as_str)
        {
            return format!("gcode {}", line.replace('\n', " ").trim());
        }
        return name.into();
    }
    "command".into()
}

/// Merges the "print" object of a report into the cache. Anything else is
/// ignored.
pub fn handle_report(cache: &StateCache, payload: &[u8]) {
    if let Ok(Value::Object(mut report)) = serde_json::from_slice(payload)
        && let Some(Value::Object(fields)) = report.remove("print")
    {
        cache.merge(&fields);
    }
}

/// Commands waiting on the broker, so each can be marked acknowledged in the
/// event log. Publishes leave in the order they were queued, so the next one
/// out is the front of `queued`; QoS 1 ones then wait on their packet id.
#[derive(Default)]
struct Pending {
    queued: VecDeque<(Option<i64>, Instant)>,
    awaiting: HashMap<u16, (Option<i64>, Instant)>,
}

struct Shared {
    host: String,
    serial: String,
    cache: StateCache,
    log: Log,
    clock: Clock,
    client: AsyncClient,
    seq: AtomicI64,
    pending: Mutex<Pending>,
}

/// One connection to one printer. Dropping it closes the connection.
pub struct Client {
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.shared.client.try_disconnect();
        self.task.abort();
    }
}

impl Client {
    pub fn start(
        host: &str,
        port: u16,
        serial: &str,
        access_code: &str,
        cache: StateCache,
        log: Log,
        clock: Clock,
    ) -> Client {
        let (client, events) = AsyncClient::new(options(host, port, access_code), 64);
        let shared = Arc::new(Shared {
            host: host.to_string(),
            serial: serial.to_string(),
            cache,
            log,
            clock,
            client,
            seq: AtomicI64::new(0),
            pending: Mutex::default(),
        });
        let task = tokio::spawn(run(shared.clone(), events));
        Client { shared, task }
    }

    fn send(&self, namespace: &str, command: &str, fields: &[(&str, Value)]) {
        let seq = self.shared.seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.shared
            .publish(QoS::AtMostOnce, request(seq, namespace, command, fields));
    }

    /// Pause, resume, stop and unload go at QoS 1: losing a Stop is worse than
    /// sending it twice. Everything else is QoS 0, being either harmless to
    /// lose (a temperature) or unsafe to repeat (extrude).
    fn send_print_command(&self, command: &str) {
        let seq = self.shared.seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.shared
            .publish(QoS::AtLeastOnce, request(seq, "print", command, &[]));
    }

    fn gcode(&self, gcode: &str) {
        self.send("print", "gcode_line", &[("param", json!(gcode))]);
    }

    pub fn lower_bed(&self) {
        self.gcode(BED_DROP_GCODE);
    }
    pub fn home(&self) {
        self.gcode(HOME_GCODE);
    }
    pub fn set_bed_temp(&self, t: i64) {
        self.gcode(&format!("M140 S{t}\n"));
    }
    pub fn set_nozzle_temp(&self, t: i64) {
        self.gcode(&format!("M104 S{t}\n"));
    }
    /// Pushes 20mm of filament for purging or cold pulls: relative extrusion,
    /// slow enough not to skip. The caller checks the nozzle is hot.
    pub fn extrude(&self) {
        self.gcode(EXTRUDE_GCODE);
    }
    /// Ejects the loaded filament back to the AMS (or out the top for an
    /// external spool). Payload from OpenBambuAPI; unverified on this printer.
    pub fn unload_filament(&self) {
        self.send_print_command("unload_filament");
    }
    pub fn pause(&self) {
        self.send_print_command("pause");
    }
    pub fn resume(&self) {
        self.send_print_command("resume");
    }
    pub fn stop(&self) {
        self.send_print_command("stop");
    }

    /// Writes one AMS tray's profile. A full-tray write, so every field is
    /// sent; `tray_info_idx` is round-tripped from the last report so a colour
    /// edit doesn't clobber it. Payload from OpenBambuAPI.
    #[allow(clippy::too_many_arguments)]
    pub fn set_ams_filament(
        &self,
        ams_id: i64,
        tray_id: i64,
        info_idx: &str,
        color: &str,
        kind: &str,
        min: i64,
        max: i64,
    ) {
        self.send(
            "print",
            "ams_filament_setting",
            &[
                ("ams_id", json!(ams_id)),
                ("tray_id", json!(tray_id)),
                ("tray_info_idx", json!(info_idx)),
                ("tray_color", json!(color)),
                ("nozzle_temp_min", json!(min)),
                ("nozzle_temp_max", json!(max)),
                ("tray_type", json!(kind)),
            ],
        );
    }

    /// Turns the chamber LED on or off. The timing fields only matter for
    /// flashing, but are sent to match the documented payload.
    pub fn set_chamber_light(&self, on: bool) {
        self.send(
            "system",
            "ledctrl",
            &[
                ("led_node", json!("chamber_light")),
                ("led_mode", json!(if on { "on" } else { "off" })),
                ("led_on_time", json!(500)),
                ("led_off_time", json!(500)),
                ("loop_times", json!(1)),
                ("interval_time", json!(1000)),
            ],
        );
    }
}

impl Shared {
    /// Logs the command, then queues it. The acknowledgement is recorded when
    /// the event loop sees it leave (QoS 0) or sees the broker confirm it.
    fn publish(&self, qos: QoS, payload: String) {
        let entry = self
            .log
            .record(activity::COMMAND, &summarize(&payload), &payload);
        let topic = format!("device/{}/request", self.serial);
        // Held across the queueing, so the event loop can't see this publish
        // leave before it is in the queue.
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        match self.client.try_publish(topic, qos, false, payload) {
            Ok(()) => pending.queued.push_back((entry, Instant::now())),
            Err(err) => {
                drop(pending);
                self.log.acknowledge(entry, Err(err.to_string()));
            }
        }
    }

    fn on_event(&self, event: Event) {
        match event {
            Event::Incoming(Packet::ConnAck(_)) => {
                self.cache.set_connected(true);
                let topic = format!("device/{}/report", self.serial);
                if let Err(err) = self.client.try_subscribe(topic, QoS::AtMostOnce) {
                    tracing::warn!("mqtt: subscribing: {err}");
                }
                self.publish(QoS::AtMostOnce, PUSHALL.into());
            }
            Event::Incoming(Packet::Publish(msg)) => {
                self.log.record(
                    activity::REPORT,
                    "report",
                    &String::from_utf8_lossy(&msg.payload),
                );
                handle_report(&self.cache, &msg.payload);
            }
            Event::Incoming(Packet::PubAck(ack)) => {
                let entry = self.lock_pending().awaiting.remove(&ack.pkid);
                if let Some((entry, _)) = entry {
                    self.log.acknowledge(entry, Ok((self.clock)()));
                }
            }
            Event::Outgoing(Outgoing::Publish(pkid)) => {
                let mut pending = self.lock_pending();
                // A QoS 1 id already waiting is a resend after a reconnect.
                if pkid != 0 && pending.awaiting.contains_key(&pkid) {
                    return;
                }
                let Some((entry, queued)) = pending.queued.pop_front() else {
                    return;
                };
                if pkid == 0 {
                    drop(pending);
                    self.log.acknowledge(entry, Ok((self.clock)()));
                } else {
                    pending.awaiting.insert(pkid, (entry, queued));
                }
            }
            _ => {}
        }
    }

    /// Records every command that has waited too long as unacknowledged.
    fn expire(&self) {
        let expired: Vec<Option<i64>> = {
            let mut pending = self.lock_pending();
            let old = |queued: &Instant| queued.elapsed() >= ACK_TIMEOUT;
            let mut out = Vec::new();
            while pending.queued.front().is_some_and(|(_, q)| old(q)) {
                out.push(pending.queued.pop_front().unwrap().0);
            }
            pending.awaiting.retain(|_, (entry, q)| {
                let keep = !old(q);
                if !keep {
                    out.push(*entry);
                }
                keep
            });
            out
        };
        for entry in expired {
            self.log
                .acknowledge(entry, Err("no acknowledgement".into()));
        }
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Drives the connection: reconnects with backoff, and hands every event to
/// `on_event`.
async fn run(shared: Arc<Shared>, mut events: EventLoop) {
    let mut delay = Duration::from_secs(1);
    let mut sweep = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            event = events.poll() => match event {
                Ok(event) => {
                    if matches!(event, Event::Incoming(Packet::ConnAck(_))) {
                        delay = Duration::from_secs(1);
                    }
                    shared.on_event(event);
                }
                Err(err) => {
                    shared.cache.set_disconnected(describe(&err, &shared.host));
                    tracing::info!("mqtt: {err}; retrying in {delay:?}");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(MAX_RECONNECT_DELAY);
                }
            },
            _ = sweep.tick() => shared.expire(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_payloads_are_exact() {
        assert_eq!(
            request(7, "print", "pause", &[]),
            r#"{"print":{"command":"pause","sequence_id":"7"}}"#
        );
        assert_eq!(
            request(
                1,
                "print",
                "ams_filament_setting",
                &[
                    ("ams_id", json!(0)),
                    ("tray_id", json!(1)),
                    ("tray_info_idx", json!("GFA00")),
                    ("tray_color", json!("FF6B35FF")),
                    ("nozzle_temp_min", json!(190)),
                    ("nozzle_temp_max", json!(230)),
                    ("tray_type", json!("PLA")),
                ]
            ),
            r#"{"print":{"ams_id":0,"command":"ams_filament_setting","nozzle_temp_max":230,"nozzle_temp_min":190,"sequence_id":"1","tray_color":"FF6B35FF","tray_id":1,"tray_info_idx":"GFA00","tray_type":"PLA"}}"#
        );
    }

    #[test]
    fn summaries() {
        assert_eq!(
            summarize(&request(
                1,
                "print",
                "gcode_line",
                &[("param", json!(BED_DROP_GCODE))]
            )),
            "gcode G90 G1 Z200 F900"
        );
        assert_eq!(summarize(&request(1, "print", "stop", &[])), "stop");
        assert_eq!(summarize(&request(1, "system", "ledctrl", &[])), "ledctrl");
        assert_eq!(summarize(PUSHALL), "pushall");
        assert_eq!(summarize("not json"), "command");
        assert_eq!(summarize("{}"), "command");
    }

    #[test]
    fn only_print_reports_are_merged() {
        let cache = StateCache::new();
        handle_report(&cache, br#"{"print":{"bed_temper":60},"info":{"x":1}}"#);
        handle_report(&cache, b"garbage");
        handle_report(&cache, br#"{"system":{"led_mode":"on"}}"#);
        assert_eq!(
            *cache.snapshot().fields,
            serde_json::from_str(r#"{"bed_temper":60}"#).unwrap()
        );
    }
}
