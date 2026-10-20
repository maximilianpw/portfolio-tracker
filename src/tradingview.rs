use std::collections::BTreeSet;

use chrono::{DateTime, SecondsFormat, Utc};
use rust_decimal::Decimal;

use crate::error::Error;
use crate::instruments::Instruments;
use crate::ledger::{Account, Kind, Posting, Status};

const CASH: &str = "$CASH";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub csv: String,
    pub rows: usize,
    pub skipped: Vec<String>,
}

struct Row {
    symbol: String,
    side: &'static str,
    qty: Decimal,
    price: Decimal,
    commission: Decimal,
    time: String,
}

fn closing_time(ts: &str) -> Result<String, Error> {
    DateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.f%z")
        .map(|t| {
            t.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Millis, true)
        })
        .map_err(|e| Error::PytrOutput(format!("unreadable timestamp {ts:?}: {e}")))
}

fn number(d: Decimal) -> String {
    d.normalize().to_string()
}

/// Builds the TradingView import CSV for one account. The output is byte-for-byte stable for the
/// same postings, because TradingView's merge only skips rows whose fields match exactly.
pub fn export(
    postings: &[Posting],
    account: Account,
    instruments: &Instruments,
) -> Result<Export, Error> {
    let mut rows = Vec::new();
    let mut skipped = Vec::new();
    let mut unmapped = BTreeSet::new();
    let mut missing_cost = BTreeSet::new();

    let mut selected: Vec<&Posting> = postings
        .iter()
        .filter(|p| p.account == account && p.status == Status::Active)
        .collect();
    selected.sort_by(|a, b| {
        (a.date, &a.ts, &a.event_id, a.leg).cmp(&(b.date, &b.ts, &b.event_id, b.leg))
    });

    for p in selected {
        let time = closing_time(&p.ts)?;
        let cash = |side: &'static str, qty: Decimal| Row {
            symbol: CASH.into(),
            side,
            qty,
            price: Decimal::ZERO,
            commission: Decimal::ZERO,
            time: time.clone(),
        };
        let mut instrument = |side: &'static str,
                              qty: Decimal,
                              price: Decimal,
                              commission: Decimal|
         -> Option<Row> {
            let isin = p.isin.as_deref()?;
            match instruments.tradingview(isin) {
                Some(symbol) => Some(Row {
                    symbol: symbol.to_owned(),
                    side,
                    qty,
                    price,
                    commission,
                    time: time.clone(),
                }),
                None => {
                    unmapped.insert(isin.to_owned());
                    None
                }
            }
        };

        let describe = || format!("{} leg {} ({})", p.event_id, p.leg, p.kind.as_str());

        // TradingView has no transfer side, so shares that arrived from elsewhere are bought at
        // their stated cost with a matching deposit, leaving the portfolio's cash unchanged.
        if matches!(p.kind, Kind::OpeningBalance | Kind::Adjustment) {
            match (p.quantity, p.price) {
                (Some(q), Some(price)) => {
                    let (trade, money) = if q > Decimal::ZERO {
                        ("buy", "deposit")
                    } else {
                        ("sell", "withdrawal")
                    };
                    if let Some(row) = instrument(trade, q.abs(), price, Decimal::ZERO) {
                        rows.push(cash(money, (q.abs() * price).round_dp(2)));
                        rows.push(row);
                    }
                }
                (Some(_), None) if p.kind == Kind::OpeningBalance => {
                    missing_cost.extend(p.isin.clone());
                }
                (Some(_), None) => skipped.push(describe()),
                (None, _) if p.amount > Decimal::ZERO => rows.push(cash("deposit", p.amount)),
                (None, _) if p.amount < Decimal::ZERO => {
                    rows.push(cash("withdrawal", p.amount.abs()))
                }
                (None, _) => {}
            }
            continue;
        }

        let row = match (p.kind, p.isin.is_some(), p.quantity) {
            (Kind::Buy, true, Some(q)) => {
                instrument("buy", q.abs(), p.price.unwrap_or_default(), p.fee + p.tax)
            }
            (Kind::Sell, true, Some(q)) => {
                instrument("sell", q.abs(), p.price.unwrap_or_default(), p.fee + p.tax)
            }
            (Kind::Dividend, true, _) if !p.amount.is_zero() => {
                instrument("dividend", p.amount, Decimal::ZERO, Decimal::ZERO)
            }
            (Kind::Dividend, _, _) => None,
            (Kind::Reward, true, Some(q)) => {
                instrument("reward", q, p.price.unwrap_or_default(), Decimal::ZERO)
            }
            (Kind::Reward | Kind::Interest, false, _) => Some(cash("income", p.amount)),
            (Kind::Deposit, _, _) => Some(cash("deposit", p.amount)),
            (Kind::Withdrawal, _, _) => Some(cash("withdrawal", p.amount.abs())),
            (Kind::Fee | Kind::Tax, false, _) => Some(cash("taxes and fees", p.amount.abs())),
            (Kind::InternalTransfer, _, _) if p.amount > Decimal::ZERO => {
                Some(cash("deposit", p.amount))
            }
            (Kind::InternalTransfer, _, _) => Some(cash("withdrawal", p.amount.abs())),
            _ => {
                skipped.push(describe());
                None
            }
        };
        rows.extend(row);
    }

    if !missing_cost.is_empty() {
        return Err(Error::MissingCost {
            isins: missing_cost.into_iter().collect::<Vec<_>>().join(", "),
        });
    }
    if !unmapped.is_empty() {
        return Err(Error::UnmappedInstruments {
            path: instruments.path.clone(),
            isins: unmapped.into_iter().collect::<Vec<_>>().join(", "),
        });
    }

    let mut writer = csv::WriterBuilder::new()
        .terminator(csv::Terminator::Any(b'\n'))
        .from_writer(Vec::new());
    writer.write_record([
        "Symbol",
        "Side",
        "Qty",
        "Fill Price",
        "Commission",
        "Closing Time",
    ])?;
    for r in &rows {
        writer.write_record([
            r.symbol.as_str(),
            r.side,
            &number(r.qty),
            &number(r.price),
            &number(r.commission),
            &r.time,
        ])?;
    }
    let csv = String::from_utf8(writer.into_inner().map_err(|e| Error::Io(e.into_error()))?)
        .map_err(|e| Error::PytrOutput(e.to_string()))?;
    Ok(Export {
        csv,
        rows: rows.len(),
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::instruments::Instrument;
    use crate::testing::d;

    fn posting(
        kind: Kind,
        account: Account,
        isin: Option<&str>,
        qty: Option<&str>,
        price: Option<&str>,
        amount: &str,
    ) -> Posting {
        Posting {
            event_id: "e1".into(),
            leg: 0,
            account,
            ts: "2026-01-05T10:00:00.123+0000".into(),
            date: "2026-01-05".parse().unwrap(),
            isin: isin.map(str::to_owned),
            instrument_name: None,
            kind,
            quantity: qty.map(d),
            price: price.map(d),
            amount: d(amount),
            fee: d("1.06"),
            tax: if kind == Kind::Buy {
                d("0.44")
            } else {
                Decimal::ZERO
            },
            saving_plan: false,
            status: Status::Active,
        }
    }

    fn instruments() -> Instruments {
        Instruments {
            path: "instruments.toml".into(),
            by_isin: BTreeMap::from([(
                "FR0000131104".to_owned(),
                Instrument {
                    tradingview: Some("EURONEXT:BNP".into()),
                    ..Instrument::default()
                },
            )]),
        }
    }

    #[test]
    fn writes_trades_dividends_and_cash() {
        let postings = vec![
            posting(Kind::Deposit, Account::Regular, None, None, None, "200"),
            posting(
                Kind::Buy,
                Account::Regular,
                Some("FR0000131104"),
                Some("2"),
                Some("55.40"),
                "-112.30",
            ),
            posting(
                Kind::Dividend,
                Account::Regular,
                Some("FR0000131104"),
                None,
                None,
                "1.50",
            ),
            posting(
                Kind::InternalTransfer,
                Account::Regular,
                None,
                None,
                None,
                "-20",
            ),
            posting(Kind::InternalTransfer, Account::Pea, None, None, None, "20"),
        ];
        let out = export(&postings, Account::Regular, &instruments()).unwrap();
        assert_eq!(
            out.csv,
            "Symbol,Side,Qty,Fill Price,Commission,Closing Time\n\
             $CASH,deposit,200,0,0,2026-01-05T10:00:00.123Z\n\
             EURONEXT:BNP,buy,2,55.4,1.5,2026-01-05T10:00:00.123Z\n\
             EURONEXT:BNP,dividend,1.5,0,0,2026-01-05T10:00:00.123Z\n\
             $CASH,withdrawal,20,0,0,2026-01-05T10:00:00.123Z\n"
        );
        assert_eq!(
            export(&postings, Account::Regular, &instruments()).unwrap(),
            out
        );
    }

    #[test]
    fn exports_transferred_shares_as_buy_with_matching_deposit() {
        let mut opening = posting(
            Kind::OpeningBalance,
            Account::Pea,
            Some("FR0000131104"),
            Some("4"),
            Some("50"),
            "0",
        );
        opening.fee = Decimal::ZERO;
        let mut cash = posting(Kind::Adjustment, Account::Pea, None, None, None, "-12.5");
        cash.fee = Decimal::ZERO;
        let mut unpriced = posting(
            Kind::Adjustment,
            Account::Pea,
            Some("FR0000131104"),
            Some("-0.3"),
            None,
            "0",
        );
        unpriced.event_id = "e2".into();
        let out = export(&[opening, cash, unpriced], Account::Pea, &instruments()).unwrap();
        assert_eq!(
            out.csv,
            "Symbol,Side,Qty,Fill Price,Commission,Closing Time\n\
             $CASH,deposit,200,0,0,2026-01-05T10:00:00.123Z\n\
             EURONEXT:BNP,buy,4,50,0,2026-01-05T10:00:00.123Z\n\
             $CASH,withdrawal,12.5,0,0,2026-01-05T10:00:00.123Z\n"
        );
        assert_eq!(out.skipped, vec!["e2 leg 0 (adjustment)".to_owned()]);
    }

    #[test]
    fn fails_on_transferred_shares_without_cost() {
        let opening = posting(
            Kind::OpeningBalance,
            Account::Pea,
            Some("FR0000131104"),
            Some("4"),
            None,
            "0",
        );
        let err = export(&[opening], Account::Pea, &instruments()).unwrap_err();
        assert!(matches!(err, Error::MissingCost { isins } if isins == "FR0000131104"));
    }

    #[test]
    fn fails_on_unmapped_instruments() {
        let postings = vec![posting(
            Kind::Buy,
            Account::Pea,
            Some("US0378331005"),
            Some("1"),
            Some("1"),
            "-1",
        )];
        let err = export(&postings, Account::Pea, &instruments()).unwrap_err();
        assert!(matches!(err, Error::UnmappedInstruments { isins, .. } if isins == "US0378331005"));
    }
}
