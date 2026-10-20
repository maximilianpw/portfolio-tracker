use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Error;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instrument {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tradingview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venue: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote_currency: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Instruments {
    pub path: String,
    pub by_isin: BTreeMap<String, Instrument>,
}

impl Instruments {
    pub fn load(path: &Path) -> Result<Instruments, Error> {
        let text = std::fs::read_to_string(path)?;
        let by_isin = toml::from_str(&text).map_err(|e| Error::InstrumentsFile {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        Ok(Instruments {
            path: path.display().to_string(),
            by_isin,
        })
    }

    pub fn tradingview(&self, isin: &str) -> Option<&str> {
        self.by_isin
            .get(isin)
            .and_then(|i| i.tradingview.as_deref())
            .filter(|s| !s.trim().is_empty())
    }

    pub fn template(&self, names: &BTreeMap<String, String>) -> String {
        let mut merged = self.by_isin.clone();
        for (isin, name) in names {
            let entry = merged.entry(isin.clone()).or_default();
            entry.name.get_or_insert_with(|| name.clone());
            entry.tradingview.get_or_insert_with(String::new);
        }
        toml::to_string(&merged).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_keeps_existing_symbols() {
        let mut by_isin = BTreeMap::new();
        by_isin.insert(
            "US0378331005".to_owned(),
            Instrument {
                name: Some("Apple".into()),
                tradingview: Some("NASDAQ:AAPL".into()),
                ..Instrument::default()
            },
        );
        let instruments = Instruments {
            path: "x".into(),
            by_isin,
        };
        let names = BTreeMap::from([
            ("US0378331005".to_owned(), "Apple".to_owned()),
            ("FR0000131104".to_owned(), "BNP Paribas".to_owned()),
        ]);
        let parsed: BTreeMap<String, Instrument> =
            toml::from_str(&instruments.template(&names)).unwrap();
        assert_eq!(
            parsed["US0378331005"].tradingview.as_deref(),
            Some("NASDAQ:AAPL")
        );
        assert_eq!(parsed["FR0000131104"].tradingview.as_deref(), Some(""));
        assert_eq!(parsed["FR0000131104"].name.as_deref(), Some("BNP Paribas"));
    }
}
