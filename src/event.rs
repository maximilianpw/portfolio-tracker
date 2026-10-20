use chrono::{DateTime, FixedOffset, NaiveDate};
use chrono_tz::Europe::Berlin;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;

use crate::numbers::{Amount, decimal_from_json_number, find_isin, is_valid_isin, parse_amount};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawEvent {
    pub id: String,
    pub timestamp: String,
    #[serde(default)]
    pub title: String,
    pub subtitle: Option<String>,
    pub event_type: Option<String>,
    pub status: Option<String>,
    pub cash_account_number: Option<String>,
    pub icon: Option<String>,
    pub amount: Option<RawMoney>,
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawMoney {
    pub value: f64,
    pub currency: String,
    #[serde(default = "default_fraction_digits")]
    pub fraction_digits: u32,
}

fn default_fraction_digits() -> u32 {
    2
}

#[derive(Debug, Clone)]
pub struct Row<'a> {
    pub section: &'a str,
    pub label: &'a str,
    pub text: Option<&'a str>,
}

impl RawEvent {
    pub fn event_type(&self) -> &str {
        self.event_type.as_deref().unwrap_or("")
    }

    pub fn timestamp(&self) -> Option<DateTime<FixedOffset>> {
        DateTime::parse_from_str(&self.timestamp, "%Y-%m-%dT%H:%M:%S%.f%z").ok()
    }

    pub fn local_date(&self) -> Option<NaiveDate> {
        self.timestamp()
            .map(|ts| ts.with_timezone(&Berlin).date_naive())
    }

    pub fn amount_eur(&self) -> Option<Decimal> {
        let money = self.amount.as_ref()?;
        if money.currency != "EUR" {
            return None;
        }
        decimal_from_json_number(money.value, money.fraction_digits)
    }

    fn sections(&self) -> impl Iterator<Item = &Value> {
        self.details
            .as_ref()
            .and_then(|d| d.get("sections"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    }

    pub fn rows(&self) -> Vec<Row<'_>> {
        let mut rows = Vec::new();
        for section in self.sections() {
            let section_title = section.get("title").and_then(Value::as_str).unwrap_or("");
            let Some(data) = section.get("data").and_then(Value::as_array) else {
                continue;
            };
            for row in data {
                let Some(label) = row.get("title").and_then(Value::as_str) else {
                    continue;
                };
                let text = row
                    .get("detail")
                    .and_then(|d| d.get("text"))
                    .and_then(Value::as_str);
                rows.push(Row {
                    section: section_title,
                    label,
                    text,
                });
            }
        }
        rows
    }

    /// An empty `sections` matches rows in any section.
    pub fn row_text(&self, sections: &[&str], labels: &[&str]) -> Option<&str> {
        self.rows()
            .into_iter()
            .find(|row| {
                (sections.is_empty() || sections.contains(&row.section))
                    && labels.contains(&row.label)
            })
            .and_then(|row| row.text)
    }

    pub fn row_amount(&self, sections: &[&str], labels: &[&str]) -> Option<Amount> {
        self.row_text(sections, labels).and_then(parse_amount)
    }

    /// The traded instrument. For derivatives the icon shows the underlying, so the header's
    /// instrument link and the row's `instrumentId` take precedence over it.
    pub fn isin(&self) -> Option<String> {
        let header = self.sections().find_map(|s| {
            let action = s.get("action")?;
            (action.get("type")?.as_str()? == "instrumentDetail")
                .then(|| action.get("payload")?.as_str())
                .flatten()
                .filter(|p| is_valid_isin(p))
                .map(str::to_owned)
        });
        if header.is_some() {
            return header;
        }

        let row_instrument = self.sections().find_map(|s| {
            s.get("data")?.as_array()?.iter().find_map(|row| {
                let id = row
                    .pointer("/detail/action/payload/contextParams/instrumentId")?
                    .as_str()?;
                is_valid_isin(id).then(|| id.to_owned())
            })
        });
        if row_instrument.is_some() {
            return row_instrument;
        }

        if let Some(isin) = self.icon.as_deref().and_then(find_isin) {
            return Some(isin);
        }

        self.details
            .as_ref()
            .and_then(|d| find_isin(&d.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::event;

    #[test]
    fn prefers_header_instrument_over_icon() {
        let e = event(
            "TRADING_TRADE_EXECUTED",
            serde_json::json!({
                "icon": "logos/US88160R1014/v2",
                "details": {"sections": [
                    {"type": "header", "title": "x", "action": {"type": "instrumentDetail", "payload": "DE000XS00019"}},
                ]}
            }),
        );
        assert_eq!(e.isin().as_deref(), Some("DE000XS00019"));
    }

    #[test]
    fn falls_back_to_icon() {
        let e = event(
            "SSP_CORPORATE_ACTION_CASH",
            serde_json::json!({"icon": "logos/US0378331005/v2"}),
        );
        assert_eq!(e.isin().as_deref(), Some("US0378331005"));
    }

    #[test]
    fn converts_timestamp_to_berlin_date() {
        let e = event(
            "INTEREST_PAYOUT",
            serde_json::json!({"timestamp": "2026-03-01T23:30:00.000+0000"}),
        );
        assert_eq!(e.local_date(), NaiveDate::from_ymd_opt(2026, 3, 2));
    }
}
