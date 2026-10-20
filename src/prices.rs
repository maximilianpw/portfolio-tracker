use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use chrono_tz::Europe::Berlin;
use rust_decimal::Decimal;
use sqlx::SqlitePool;

use crate::db::{PriceRow, upsert_price};
use crate::error::Error;
use crate::pytr::Pytr;

#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    pub isin: String,
    pub price: Decimal,
    pub currency: String,
    pub basis: &'static str,
    pub venue: Option<String>,
}

pub trait PriceSource {
    fn name(&self) -> &'static str;
    fn fetch(&self, isins: &[String]) -> impl Future<Output = Result<Vec<Quote>, Error>> + Send;
}

pub struct PytrRates {
    pub pytr: Pytr,
    pub dir: PathBuf,
}

impl PriceSource for PytrRates {
    fn name(&self) -> &'static str {
        "pytr"
    }

    /// pytr quotes in EUR and doesn't say which venue answered.
    async fn fetch(&self, isins: &[String]) -> Result<Vec<Quote>, Error> {
        Ok(self
            .pytr
            .rates(&self.dir, isins)
            .await?
            .into_iter()
            .map(|r| Quote {
                isin: r.isin,
                price: r.price,
                currency: "EUR".into(),
                basis: "last",
                venue: None,
            })
            .collect())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CollectStats {
    pub stored: usize,
    pub missing: Vec<String>,
    pub rejected: Vec<String>,
}

pub async fn collect(
    pool: &SqlitePool,
    source: &impl PriceSource,
    isins: &[String],
    observed_at: DateTime<Utc>,
) -> Result<CollectStats, Error> {
    let quotes = source.fetch(isins).await?;
    let date = observed_at.with_timezone(&Berlin).date_naive();
    let mut stats = CollectStats::default();
    let mut seen = BTreeSet::new();
    for q in quotes {
        if q.currency != "EUR" {
            stats.rejected.push(q.isin);
            continue;
        }
        seen.insert(q.isin.clone());
        upsert_price(
            pool,
            &PriceRow {
                isin: q.isin,
                date,
                price: q.price,
                currency: q.currency,
                basis: q.basis.into(),
                observed_at: observed_at.to_rfc3339(),
                venue: q.venue,
                source: source.name().into(),
            },
        )
        .await?;
        stats.stored += 1;
    }
    stats.missing = isins
        .iter()
        .filter(|i| !seen.contains(*i))
        .cloned()
        .collect();
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect, load_prices};
    use crate::testing::d;

    struct Fixed(Vec<Quote>);

    impl PriceSource for Fixed {
        fn name(&self) -> &'static str {
            "fixed"
        }

        async fn fetch(&self, _isins: &[String]) -> Result<Vec<Quote>, Error> {
            Ok(self.0.clone())
        }
    }

    fn quote(isin: &str, currency: &str) -> Quote {
        Quote {
            isin: isin.into(),
            price: d("10.5"),
            currency: currency.into(),
            basis: "last",
            venue: None,
        }
    }

    #[tokio::test]
    async fn stores_eur_quotes_and_reports_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let pool = connect(&dir.path().join("db.sqlite")).await.unwrap();
        let source = Fixed(vec![
            quote("US0378331005", "EUR"),
            quote("IE00B4L5Y983", "USD"),
        ]);
        let isins = vec![
            "US0378331005".to_owned(),
            "IE00B4L5Y983".to_owned(),
            "FR0000131104".to_owned(),
        ];
        let observed: DateTime<Utc> = "2026-01-05T22:30:00Z".parse().unwrap();

        let stats = collect(&pool, &source, &isins, observed).await.unwrap();
        assert_eq!(stats.stored, 1);
        assert_eq!(stats.rejected, vec!["IE00B4L5Y983".to_owned()]);
        assert_eq!(
            stats.missing,
            vec!["IE00B4L5Y983".to_owned(), "FR0000131104".to_owned()]
        );

        let prices = load_prices(&pool).await.unwrap();
        assert_eq!(prices.len(), 1);
        assert_eq!(prices[0].date.to_string(), "2026-01-05");
    }
}
