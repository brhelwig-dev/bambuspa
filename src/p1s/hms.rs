//! Printer errors as readable messages: HMS entries and the separate
//! print_error value.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Fields;

/// The community English message table (ha-bambulab, MIT), filtered to the
/// P1S/P1P. Not verified against a real printer payload. "hms" is keyed by
/// `format_hms_code`, "print_error" by `format_print_error`.
const MESSAGES: &str = include_str!("error_messages.json");

/// Take precedence over the table.
const HMS_OVERRIDES: &[(&str, &str)] = &[("0300-8000-0003-0002", "AMS filament runout")];

#[derive(Deserialize)]
struct Tables {
    hms: HashMap<String, String>,
    print_error: HashMap<String, String>,
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut t: Tables = serde_json::from_str(MESSAGES).expect("p1s: bad error_messages.json");
        for (code, msg) in HMS_OVERRIDES {
            t.hms.insert(code.to_string(), msg.to_string());
        }
        t
    })
}

/// An attr/code pair in Bambu's dash-grouped hex, e.g. "0300-8000-0003-0002".
pub fn format_hms_code(attr: i64, code: i64) -> String {
    let hex = format!("{attr:08X}{code:08X}");
    format!(
        "{}-{}-{}-{}",
        &hex[0..4],
        &hex[4..8],
        &hex[8..12],
        &hex[12..16]
    )
}

/// A print_error value as two dash-grouped words, e.g. "0300-4000".
pub fn format_print_error(code: i64) -> String {
    let hex = format!("{code:08X}");
    format!("{}-{}", &hex[0..4], &hex[4..8])
}

/// One translated printer error.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HmsEntry {
    pub code: String,
    pub message: String,
}

fn entry(code: String, table: &HashMap<String, String>) -> HmsEntry {
    let message = table.get(&code).cloned().unwrap_or_else(|| code.clone());
    HmsEntry { code, message }
}

/// The non-zero print_error first, then each HMS entry, translated (falling
/// back to the raw code when it isn't in the table). Entries with an
/// unexpected shape are skipped.
pub fn hms_errors(fields: &Fields) -> Vec<HmsEntry> {
    let mut out = Vec::new();
    if let Some(pe) = fields
        .get("print_error")
        .and_then(Value::as_f64)
        .filter(|&pe| pe > 0.0)
    {
        out.push(entry(format_print_error(pe as i64), &tables().print_error));
    }
    for item in fields
        .get("hms")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let attr = item.get("attr").and_then(Value::as_f64);
        let code = item.get("code").and_then(Value::as_f64);
        if let (Some(attr), Some(code)) = (attr, code) {
            out.push(entry(
                format_hms_code(attr as i64, code as i64),
                &tables().hms,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn errors(v: Value) -> Vec<HmsEntry> {
        hms_errors(v.as_object().unwrap())
    }

    #[test]
    fn formats() {
        assert_eq!(
            format_hms_code(0x03008000, 0x00030002),
            "0300-8000-0003-0002"
        );
        assert_eq!(format_print_error(0x03004000), "0300-4000");
    }

    #[test]
    fn translates_known_and_falls_back_on_unknown() {
        let got = errors(json!({"hms": [
            {"attr": 0x03008000u32, "code": 0x00030002u32},
            {"attr": 0xAAAAAAAAu32, "code": 0xBBBBBBBBu32},
        ]}));
        assert_eq!(
            got[0],
            HmsEntry {
                code: "0300-8000-0003-0002".into(),
                message: "AMS filament runout".into()
            }
        );
        assert_eq!(got[1].code, "AAAA-AAAA-BBBB-BBBB");
        assert_eq!(got[1].message, got[1].code);
    }

    #[test]
    fn odd_shapes_are_skipped() {
        for v in [
            json!({}),
            json!({"hms": "not-an-array"}),
            json!({"hms": ["not-a-map"]}),
            json!({"hms": [{"attr": "x", "code": 1}]}),
        ] {
            assert!(errors(v).is_empty());
        }
    }

    #[test]
    fn embedded_tables() {
        assert!(!tables().hms["0300-0100-0001-0001"].is_empty());
        assert!(!tables().print_error["0300-4000"].is_empty());
    }

    #[test]
    fn print_error_comes_first_and_zero_is_silent() {
        let got = errors(json!({"print_error": 0x03004000, "hms": [{"attr": 1, "code": 2}]}));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].code, "0300-4000");
        assert_ne!(got[0].message, got[0].code);
        assert!(errors(json!({"print_error": 0})).is_empty());
        assert_eq!(
            errors(json!({"print_error": 0xDEADBEEFu32}))[0].message,
            "DEAD-BEEF"
        );
    }
}
