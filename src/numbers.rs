use std::str::FromStr;

use rust_decimal::Decimal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Currency {
    Eur,
    Usd,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Amount {
    pub value: Decimal,
    pub currency: Option<Currency>,
}

const FREE_WORDS: &[&str] = &["kostenlos", "free", "gratis", "gratuit"];

/// Parses Trade Republic display numbers such as `1,00 €`, `0.5`, `1.234,56 €` or `-0,03 €`.
///
/// A lone `.` is read as a decimal point, so `1.500` means 1.5: Trade Republic only uses `.` as a
/// thousands separator together with a `,` decimal.
pub fn parse_amount(text: &str) -> Option<Amount> {
    let trimmed = text.trim();
    if FREE_WORDS.iter().any(|w| trimmed.eq_ignore_ascii_case(w)) {
        return Some(Amount {
            value: Decimal::ZERO,
            currency: None,
        });
    }

    let currency = if trimmed.contains('€') || trimmed.contains("EUR") {
        Some(Currency::Eur)
    } else if trimmed.contains('$') || trimmed.contains("USD") {
        Some(Currency::Usd)
    } else if trimmed.contains('£') || trimmed.contains("CHF") {
        Some(Currency::Other)
    } else {
        None
    };

    let cleaned: String = trimmed
        .replace('−', "-")
        .chars()
        .filter(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | '-'))
        .collect();
    if !cleaned.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }

    let normalized = match (cleaned.rfind(','), cleaned.rfind('.')) {
        (Some(comma), Some(dot)) if comma > dot => cleaned.replace('.', "").replace(',', "."),
        (Some(_), Some(_)) => cleaned.replace(',', ""),
        (Some(_), None) if cleaned.matches(',').count() > 1 => cleaned.replace(',', ""),
        (Some(_), None) => cleaned.replace(',', "."),
        (None, Some(_)) if cleaned.matches('.').count() > 1 => cleaned.replace('.', ""),
        _ => cleaned,
    };

    Decimal::from_str(&normalized)
        .ok()
        .map(|value| Amount { value, currency })
}

pub fn parse_decimal(text: &str) -> Option<Decimal> {
    parse_amount(text).map(|a| a.value)
}

/// Trade Republic sends `amount.value` as a JSON float; this recovers the exact decimal it displays.
pub fn decimal_from_json_number(value: f64, fraction_digits: u32) -> Option<Decimal> {
    Decimal::from_str(&value.to_string())
        .ok()
        .map(|d| d.round_dp(fraction_digits))
}

pub fn is_valid_isin(candidate: &str) -> bool {
    let bytes = candidate.as_bytes();
    if bytes.len() != 12
        || !bytes[..2].iter().all(u8::is_ascii_uppercase)
        || !bytes[2..11]
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        || !bytes[11].is_ascii_digit()
    {
        return false;
    }

    let digits: Vec<u32> = bytes
        .iter()
        .flat_map(|&b| {
            let v = if b.is_ascii_digit() {
                u32::from(b - b'0')
            } else {
                u32::from(b - b'A') + 10
            };
            if v >= 10 {
                vec![v / 10, v % 10]
            } else {
                vec![v]
            }
        })
        .collect();

    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            if i % 2 == 1 {
                let doubled = d * 2;
                if doubled > 9 { doubled - 9 } else { doubled }
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

pub fn find_isin(text: &str) -> Option<String> {
    text.as_bytes()
        .windows(12)
        .filter_map(|window| std::str::from_utf8(window).ok())
        .find(|window| is_valid_isin(window))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn parses_german_and_english_formats() {
        assert_eq!(parse_decimal("1,00\u{a0}€"), Some(d("1.00")));
        assert_eq!(parse_decimal("0.123456"), Some(d("0.123456")));
        assert_eq!(parse_decimal("1.234,56\u{a0}€"), Some(d("1234.56")));
        assert_eq!(parse_decimal("1,234.56 $"), Some(d("1234.56")));
        assert_eq!(parse_decimal("-0,03\u{a0}€"), Some(d("-0.03")));
        assert_eq!(parse_decimal("12.34\u{a0}€"), Some(d("12.34")));
        assert_eq!(parse_decimal("2"), Some(d("2")));
        assert_eq!(parse_decimal("Kostenlos"), Some(Decimal::ZERO));
        assert_eq!(parse_decimal("Free"), Some(Decimal::ZERO));
        assert_eq!(parse_decimal("Ausgeführt"), None);
    }

    #[test]
    fn detects_currency() {
        assert_eq!(
            parse_amount("0,01\u{a0}$").unwrap().currency,
            Some(Currency::Usd)
        );
        assert_eq!(
            parse_amount("120,00\u{a0}€").unwrap().currency,
            Some(Currency::Eur)
        );
        assert_eq!(parse_amount("0,25").unwrap().currency, None);
    }

    #[test]
    fn recovers_json_floats() {
        assert_eq!(decimal_from_json_number(45.67, 2), Some(d("45.67")));
        assert_eq!(decimal_from_json_number(-0.1 - 0.2, 2), Some(d("-0.30")));
    }

    #[test]
    fn validates_isin_checksums() {
        assert!(is_valid_isin("US0378331005"));
        assert!(is_valid_isin("FR0000131104"));
        assert!(is_valid_isin("IE00B4L5Y983"));
        assert!(!is_valid_isin("US0378331006"));
        assert!(!is_valid_isin("QB5HEKMCMDQ4"));
        assert_eq!(
            find_isin("logos/US0378331005/v2").as_deref(),
            Some("US0378331005")
        );
        assert_eq!(find_isin("logos/QB5HEKMCMDQ4/v2"), None);
    }
}
