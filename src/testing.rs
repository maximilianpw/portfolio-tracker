use std::str::FromStr;

use rust_decimal::Decimal;
use serde_json::{Value, json};

use crate::event::RawEvent;

pub fn d(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

/// A synthetic event with the given type; `fields` override the defaults.
pub fn event(event_type: &str, fields: Value) -> RawEvent {
    let mut base = json!({
        "id": format!("evt-{event_type}"),
        "timestamp": "2026-01-15T10:00:00.000+0000",
        "title": "Test",
        "eventType": event_type,
        "status": "EXECUTED",
    });
    if let (Some(base), Some(fields)) = (base.as_object_mut(), fields.as_object()) {
        base.extend(fields.clone());
    }
    serde_json::from_value(base).unwrap()
}

pub fn table(title: &str, rows: &[(&str, &str)]) -> Value {
    json!({
        "type": "table",
        "title": title,
        "data": rows.iter().map(|(label, text)| json!({"title": label, "detail": {"text": text, "type": "text"}})).collect::<Vec<_>>(),
    })
}
