use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

pub const FRAME_COUNT: &str = "X-Frame-Count";
pub const FRAME_INTERVAL: &str = "X-Frame-Interval";

pub mod settings {
    pub const TIMELAPSE_STORAGE: &str = "timelapse-storage";
    pub const BED_OFF_AFTER: &str = "bed-off-after";
    pub const NOZZLE_OFF_AFTER: &str = "nozzle-off-after";
    pub const LAMP_OFF_AFTER: &str = "lamp-off-after";
    pub const ACTIVITY_LIMIT: &str = "activity-limit";
    pub const SESSION_LENGTH: &str = "session-length";
    pub const DASHBOARD: &str = "dashboard";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerMessage {
    Printers { printers: Vec<PrinterSummary> },
    Status(Box<Status>),
    Timelapses { timelapses: Vec<Timelapse> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientMessage {
    Select { printer: i64 },
    Follow { on: bool },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Status {
    pub printer: Option<i64>,
    pub connected: bool,
    pub configured: bool,
    pub problem: Option<String>,
    pub gcode_state: String,
    pub actions_allowed: bool,
    pub bed_temp: Value,
    pub bed_target: Value,
    pub nozzle_temp: Value,
    pub nozzle_target: Value,
    pub progress: Value,
    pub job_name: Value,
    pub layer_num: Value,
    pub total_layer_num: Value,
    pub remaining_minutes: Value,
    pub chamber_light: Option<bool>,
    pub fans: Fans,
    pub ams: Value,
    #[serde(deserialize_with = "null_as_default")]
    pub hms: Vec<HmsEntry>,
    pub bed_off_in: Option<i64>,
    pub nozzle_off_in: Option<i64>,
    pub lamp_off_in: Option<i64>,
    pub print_actions: PrintActions,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fans {
    pub cooling: Value,
    pub aux: Value,
    pub chamber: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrintActions {
    pub pause: bool,
    pub resume: bool,
    pub stop: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HmsEntry {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PrinterSummary {
    pub id: i64,
    pub name: String,
    pub connected: bool,
    pub problem: Option<String>,
    pub gcode_state: String,
    pub progress: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Timelapse {
    pub name: String,
    pub bytes: i64,
}

/// How far the server has got copying a timelapse off the printer's SD card.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TimelapseDownload {
    pub bytes: i64,
    pub total: i64,
    pub ready: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PrinterInfo {
    pub id: i64,
    pub name: String,
    pub ip: String,
    pub serial: String,
    pub access_code_set: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PrinterRequest {
    pub name: String,
    pub ip: String,
    pub serial: String,
    pub access_code: String,
    pub skip_check: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Created {
    pub id: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Settings {
    pub timelapse_storage: i64,
    pub bed_off_after: i64,
    pub nozzle_off_after: i64,
    pub lamp_off_after: i64,
    pub activity_limit: i64,
    pub session_length: i64,
    pub dashboard: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Events {
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Event {
    pub id: i64,
    pub at: String,
    pub kind: String,
    pub summary: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub payload: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acked: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PushKey {
    pub key: String,
    pub subscribed: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubscribeRequest {
    pub endpoint: String,
    pub keys: Keys,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Keys {
    pub p256dh: String,
    pub auth: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EndpointRequest {
    pub endpoint: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Preferences {
    pub available: Vec<String>,
    pub kinds: Vec<String>,
    pub bed_interval: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PreferencesRequest {
    pub endpoint: String,
    pub kinds: Option<Vec<String>>,
    pub bed_interval: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TestResult {
    pub delivered: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Me {
    pub name: String,
    pub subject: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Logout {
    pub then: String,
}

fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
