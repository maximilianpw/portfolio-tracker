use std::collections::BTreeMap;
use std::path::Path;

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::db::PortfolioSnapshot;
use crate::error::Error;
use crate::ledger::{Account, Kind, Posting, Status};

const TOLERANCE: Decimal = Decimal::from_parts(1, 0, 0, false, 2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrectionKind {
    OpeningBalance,
    Adjustment,
}

/// A reviewed fix for something the timeline doesn't contain, such as securities and cash
/// that arrived with a PEA transfer. Either `isin` and `quantity` (securities) or `amount`
/// (cash) is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Correction {
    pub account: Account,
    pub date: NaiveDate,
    pub kind: CorrectionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantity: Option<Decimal>,
    /// Cost per share, used only for the TradingView export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<Decimal>,
    pub note: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default, rename = "correction")]
    corrections: Vec<Correction>,
}

pub fn load(path: &Path) -> Result<Vec<Correction>, Error> {
    let fail = |message: String| Error::CorrectionsFile {
        path: path.display().to_string(),
        message,
    };
    let text = std::fs::read_to_string(path).map_err(|e| fail(e.to_string()))?;
    let file: File = toml::from_str(&text).map_err(|e| fail(e.to_string()))?;
    for (i, c) in file.corrections.iter().enumerate() {
        let securities = c.isin.is_some() && c.quantity.is_some() && c.amount.is_none();
        let cash = c.isin.is_none() && c.quantity.is_none() && c.amount.is_some();
        if !securities && !cash {
            return Err(fail(format!(
                "correction #{} needs either isin and quantity, or amount",
                i + 1
            )));
        }
    }
    Ok(file.corrections)
}

pub fn load_optional(path: Option<&Path>) -> Result<Vec<Correction>, Error> {
    path.map(load).transpose().map(Option::unwrap_or_default)
}

pub fn to_postings(corrections: &[Correction]) -> Vec<Posting> {
    corrections
        .iter()
        .map(|c| Posting {
            event_id: format!(
                "correction:{}:{}:{}",
                c.account,
                c.date,
                c.isin.as_deref().unwrap_or("cash")
            ),
            leg: 0,
            account: c.account,
            ts: format!("{}T00:00:00.000+0000", c.date),
            date: c.date,
            isin: c.isin.clone(),
            instrument_name: None,
            kind: match c.kind {
                CorrectionKind::OpeningBalance => Kind::OpeningBalance,
                CorrectionKind::Adjustment => Kind::Adjustment,
            },
            quantity: c.quantity,
            price: c.price,
            amount: c.amount.unwrap_or_default(),
            fee: Decimal::ZERO,
            tax: Decimal::ZERO,
            saving_plan: false,
            status: Status::Active,
        })
        .collect()
}

pub fn render(corrections: &[Correction]) -> String {
    toml::to_string(&File {
        corrections: corrections.to_vec(),
    })
    .unwrap_or_default()
}

/// Proposes corrections that close the gap between the ledger and what Trade Republic reports.
///
/// A gap that one quantity at the start of the account's history can close without any running
/// total going negative becomes an opening balance; anything else becomes an adjustment dated
/// `today`, for a human to re-date from statements.
pub fn propose(
    postings: &[Posting],
    snapshots: &[(Account, PortfolioSnapshot)],
    today: NaiveDate,
) -> Vec<Correction> {
    let mut proposals = Vec::new();
    for (account, snapshot) in snapshots {
        let mut history: Vec<&Posting> = postings
            .iter()
            .filter(|p| p.account == *account && p.status == Status::Active)
            .collect();
        history.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.ts.cmp(&b.ts)));
        let Some(start) = history.first().map(|p| p.date) else {
            continue;
        };

        let mut running: BTreeMap<&str, (Decimal, Decimal)> = BTreeMap::new();
        let mut cash = (Decimal::ZERO, Decimal::ZERO);
        for p in &history {
            if let (Some(isin), Some(qty)) = (&p.isin, p.quantity) {
                let (total, low) = running.entry(isin).or_default();
                *total += qty;
                *low = (*low).min(*total);
            }
            cash.0 += p.amount;
            cash.1 = cash.1.min(cash.0);
        }

        let reported: BTreeMap<&str, &crate::db::SnapshotPosition> = snapshot
            .positions
            .iter()
            .map(|p| (p.isin.as_str(), p))
            .collect();
        let isins: std::collections::BTreeSet<&str> = running
            .keys()
            .copied()
            .chain(reported.keys().copied())
            .collect();
        for isin in isins {
            let (total, low) = running.get(isin).copied().unwrap_or_default();
            let position = reported.get(isin);
            let gap = position.map_or(Decimal::ZERO, |p| p.quantity) - total;
            if gap.abs() < Decimal::new(1, 6) {
                continue;
            }
            let opening = gap > Decimal::ZERO && low + gap >= Decimal::ZERO;
            proposals.push(Correction {
                account: *account,
                date: if opening { start } else { today },
                kind: if opening { CorrectionKind::OpeningBalance } else { CorrectionKind::Adjustment },
                isin: Some(isin.to_owned()),
                quantity: Some(gap.normalize()),
                price: None,
                amount: None,
                note: if opening {
                    "Held before the history starts (e.g. transferred in). Add price: the cost per share from the old \
                     bank's statement. Trade Republic's average buy-in is 0 or wrong for transferred shares."
                        .into()
                } else {
                    "Unexplained difference between the history and Trade Republic. Set the real date and reason."
                        .into()
                },
            });
        }

        let Some(reported_cash) = snapshot.cash else {
            continue;
        };
        let (total, low) = cash;
        let mut remaining = reported_cash - total;
        if low < -TOLERANCE {
            proposals.push(Correction {
                account: *account,
                date: start,
                kind: CorrectionKind::OpeningBalance,
                isin: None,
                quantity: None,
                price: None,
                amount: Some(-low),
                note: "The smallest starting cash that keeps the balance from going negative. Replace it \
                       with the amount from the transfer statement."
                    .into(),
            });
            remaining += low;
        }
        if remaining.abs() > TOLERANCE {
            proposals.push(Correction {
                account: *account,
                date: today,
                kind: CorrectionKind::Adjustment,
                isin: None,
                quantity: None,
                price: None,
                amount: Some(remaining),
                note: "Cash difference left after the opening balance; money moved without a timeline event. \
                       Set the real date and reason."
                    .into(),
            });
        }
    }
    proposals
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SnapshotPosition;
    use crate::testing::d;

    fn posting(
        day: &str,
        kind: Kind,
        isin: Option<&str>,
        qty: Option<&str>,
        amount: &str,
    ) -> Posting {
        Posting {
            event_id: format!("{day}-{}", kind.as_str()),
            leg: 0,
            account: Account::Pea,
            ts: format!("{day}T10:00:00.000+0000"),
            date: day.parse().unwrap(),
            isin: isin.map(str::to_owned),
            instrument_name: None,
            kind,
            quantity: qty.map(d),
            price: None,
            amount: d(amount),
            fee: Decimal::ZERO,
            tax: Decimal::ZERO,
            saving_plan: false,
            status: Status::Active,
        }
    }

    const ST: &str = "DE0005557508";
    const SP: &str = "IE00B4L5Y983";

    #[test]
    fn proposes_opening_balances_and_adjustments() {
        let postings = vec![
            posting("2025-03-03", Kind::Buy, Some(ST), Some("3"), "-70"),
            posting("2025-08-01", Kind::Sell, Some(SP), Some("-12"), "1000"),
            posting("2025-08-02", Kind::Sell, Some(ST), Some("-9"), "400"),
            posting("2025-09-01", Kind::Buy, Some(ST), Some("50"), "-1500"),
        ];
        let snapshot = PortfolioSnapshot {
            positions: vec![SnapshotPosition {
                isin: ST.into(),
                quantity: d("50"),
                avg_cost: Some(d("40")),
            }],
            cash: Some(d("0")),
        };
        let today: NaiveDate = "2026-10-20".parse().unwrap();
        let proposals = propose(&postings, &[(Account::Pea, snapshot)], today);
        let summary: Vec<_> = proposals
            .iter()
            .map(|c| {
                (
                    c.kind,
                    c.date.to_string(),
                    c.isin.as_deref(),
                    c.quantity,
                    c.price,
                    c.amount,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    CorrectionKind::OpeningBalance,
                    "2025-03-03".into(),
                    Some(ST),
                    Some(d("6")),
                    None,
                    None
                ),
                (
                    CorrectionKind::OpeningBalance,
                    "2025-03-03".into(),
                    Some(SP),
                    Some(d("12")),
                    None,
                    None
                ),
                (
                    CorrectionKind::OpeningBalance,
                    "2025-03-03".into(),
                    None,
                    None,
                    None,
                    Some(d("170"))
                ),
            ]
        );
    }

    #[test]
    fn surplus_shares_become_an_adjustment() {
        let postings = vec![posting(
            "2026-05-04",
            Kind::CorporateAction,
            Some("FR0000120321"),
            Some("0.3"),
            "0",
        )];
        let snapshot = PortfolioSnapshot {
            positions: vec![],
            cash: None,
        };
        let proposals = propose(
            &postings,
            &[(Account::Pea, snapshot)],
            "2026-10-20".parse().unwrap(),
        );
        assert_eq!(proposals.len(), 1);
        assert_eq!(
            (proposals[0].kind, proposals[0].quantity),
            (CorrectionKind::Adjustment, Some(d("-0.3")))
        );
    }

    #[test]
    fn round_trips_through_toml_and_rejects_ambiguous_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrections.toml");
        let proposals = vec![Correction {
            account: Account::Pea,
            date: "2025-03-03".parse().unwrap(),
            kind: CorrectionKind::OpeningBalance,
            isin: Some(ST.into()),
            quantity: Some(d("6")),
            price: Some(d("21.5")),
            amount: None,
            note: "transfer".into(),
        }];
        std::fs::write(&path, render(&proposals)).unwrap();
        assert_eq!(load(&path).unwrap(), proposals);

        std::fs::write(&path, "[[correction]]\naccount = \"pea\"\ndate = 2025-03-03\nkind = \"adjustment\"\nisin = \"X\"\nnote = \"\"\n").unwrap();
        assert!(matches!(load(&path), Err(Error::CorrectionsFile { .. })));
    }
}
