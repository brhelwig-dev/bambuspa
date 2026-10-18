use serde_json::Value;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = Date)]
    type LocalDate;

    #[wasm_bindgen(constructor, js_class = "Date")]
    fn new(init: &JsValue) -> LocalDate;

    #[wasm_bindgen(method, js_class = "Date", js_name = toLocaleString)]
    fn to_locale_string(this: &LocalDate, locales: &JsValue, options: &JsValue) -> String;

    #[wasm_bindgen(method, js_class = "Date", js_name = toLocaleTimeString)]
    fn to_locale_time_string(this: &LocalDate) -> String;
}

pub fn window() -> web_sys::Window {
    web_sys::window().expect("a browser window")
}

pub fn stored(key: &str) -> Option<String> {
    window().local_storage().ok()??.get_item(key).ok()?
}

pub fn store(key: &str, value: &str) {
    if let Ok(Some(storage)) = window().local_storage() {
        let _ = storage.set_item(key, value);
    }
}

pub fn query_param(name: &str) -> Option<String> {
    let search = window().location().search().ok()?;
    search
        .trim_start_matches('?')
        .split('&')
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_string())
}

pub fn here() -> String {
    let location = window().location();
    let path = location.pathname().unwrap_or_default();
    let search = location.search().unwrap_or_default();
    format!("{path}{search}")
}

pub fn go_to(url: &str) {
    let _ = window().location().set_href(url);
}

pub fn encode(text: &str) -> String {
    js_sys::encode_uri_component(text).into()
}

pub fn fixed1(v: &Value) -> String {
    match v.as_f64() {
        Some(n) => js_sys::Number::from(n)
            .to_fixed(1)
            .map(String::from)
            .unwrap_or_else(|_| "?".into()),
        None => "?".into(),
    }
}

pub fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.to_string(),
            None => n.as_f64().map(|f| f.to_string()).unwrap_or_default(),
        },
        other => other.to_string(),
    }
}

pub fn or_unknown(v: &Value) -> String {
    if v.is_null() { "?".into() } else { text(v) }
}

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

pub fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub fn parse_int(v: &Value) -> Option<i64> {
    let text = text(v);
    let text = text.trim_start();
    let digits = text
        .char_indices()
        .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && (*c == '-' || *c == '+')))
        .count();
    text[..digits].parse().ok()
}

pub fn fmt_countdown(secs: i64) -> String {
    if secs >= 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

pub fn fmt_minutes(mins: f64) -> String {
    let m = mins.floor();
    if m >= 60.0 {
        format!("{}h {}m", (m / 60.0).floor(), m % 60.0)
    } else {
        format!("{m} min")
    }
}

pub fn cam_time(ts: f64, with_seconds: bool) -> String {
    let options = if with_seconds {
        r#"{"month":"short","day":"numeric","hour":"2-digit","minute":"2-digit","second":"2-digit"}"#
    } else {
        r#"{"month":"short","day":"numeric","hour":"2-digit","minute":"2-digit"}"#
    };
    let options = js_sys::JSON::parse(options).unwrap_or(JsValue::UNDEFINED);
    LocalDate::new(&JsValue::from_f64(ts * 1000.0)).to_locale_string(&JsValue::UNDEFINED, &options)
}

pub fn local_time(at: &str) -> String {
    LocalDate::new(&JsValue::from_str(at)).to_locale_time_string()
}
