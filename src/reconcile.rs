use std::collections::BTreeMap;

use chrono::NaiveDate;
use rust_decimal::Decimal;

use crate::db::{PortfolioSnapshot, ReconciliationIssue, SnapshotPosition};
use crate::ledger::Account;
use crate::valuation::{Ledger, Scope};

const QUANTITY_TOLERANCE: Decimal = Decimal::from_parts(1, 0, 0, false, 6);
const CASH_TOLERANCE: Decimal = Decimal::from_parts(1, 0, 0, false, 2);

pub fn reconcile(
    ledger: &Ledger,
    account: Account,
    date: NaiveDate,
    snapshot: &PortfolioSnapshot,
) -> Vec<ReconciliationIssue> {
    let derived: BTreeMap<String, Decimal> = ledger
        .holdings(Scope::Account(account), date)
        .into_iter()
        .map(|((_, isin), qty)| (isin, qty))
        .collect();
    let reported: BTreeMap<&str, &SnapshotPosition> = snapshot
        .positions
        .iter()
        .map(|p| (p.isin.as_str(), p))
        .collect();

    let issue = |isin: Option<&str>,
                 derived: Option<Decimal>,
                 reported: Option<Decimal>,
                 issue: &str| ReconciliationIssue {
        account,
        isin: isin.map(str::to_owned),
        derived,
        reported,
        issue: issue.to_owned(),
    };

    let mut issues = Vec::new();
    for (isin, &qty) in derived.iter().filter(|(_, q)| **q > Decimal::ZERO) {
        match reported.get(isin.as_str()) {
            None => issues.push(issue(
                Some(isin),
                Some(qty),
                None,
                "missing_in_trade_republic",
            )),
            Some(SnapshotPosition { quantity: r, .. }) if (qty - r).abs() > QUANTITY_TOLERANCE => {
                issues.push(issue(Some(isin), Some(qty), Some(*r), "quantity_mismatch"));
            }
            Some(_) => {}
        }
    }
    for (isin, p) in &reported {
        if !derived.contains_key(*isin) {
            issues.push(issue(
                Some(isin),
                None,
                Some(p.quantity),
                "missing_in_ledger",
            ));
        }
    }

    if let Some(reported_cash) = snapshot.cash {
        let derived_cash = ledger.cash(Scope::Account(account), date);
        if (derived_cash - reported_cash).abs() > CASH_TOLERANCE {
            issues.push(issue(
                None,
                Some(derived_cash),
                Some(reported_cash),
                "cash_mismatch",
            ));
        }
    }
    issues
}

/// Checks that need no snapshot: Trade Republic allows neither negative holdings nor negative
/// cash, so either means events are missing from the timeline.
pub fn consistency(ledger: &Ledger, account: Account, date: NaiveDate) -> Vec<ReconciliationIssue> {
    let mut issues: Vec<ReconciliationIssue> = ledger
        .holdings(Scope::Account(account), date)
        .into_iter()
        .filter(|(_, qty)| *qty < -QUANTITY_TOLERANCE)
        .map(|((_, isin), qty)| ReconciliationIssue {
            account,
            isin: Some(isin),
            derived: Some(qty),
            reported: None,
            issue: "negative_quantity".to_owned(),
        })
        .collect();
    let cash = ledger.cash(Scope::Account(account), date);
    if cash < -CASH_TOLERANCE {
        issues.push(ReconciliationIssue {
            account,
            isin: None,
            derived: Some(cash),
            reported: None,
            issue: "negative_cash".to_owned(),
        });
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{Kind, Posting, Status};
    use crate::testing::d;

    fn ledger() -> Ledger {
        let posting = |kind, isin: Option<&str>, qty: Option<&str>, amount: &str| Posting {
            event_id: amount.to_string(),
            leg: 0,
            account: Account::Regular,
            ts: "2026-01-05T10:00:00.000+0000".into(),
            date: "2026-01-05".parse().unwrap(),
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
        };
        Ledger::new(
            vec![
                posting(Kind::Deposit, None, None, "100"),
                posting(Kind::Buy, Some("US0378331005"), Some("1.5"), "-60"),
                posting(Kind::Buy, Some("DE000XS00019"), Some("3"), "-30"),
            ],
            vec![],
            BTreeMap::new(),
        )
    }

    #[test]
    fn reports_mismatches() {
        let snapshot = PortfolioSnapshot {
            positions: vec![
                SnapshotPosition {
                    isin: "US0378331005".into(),
                    quantity: d("1.4"),
                    avg_cost: None,
                },
                SnapshotPosition {
                    isin: "DE000XS00019".into(),
                    quantity: d("3"),
                    avg_cost: None,
                },
                SnapshotPosition {
                    isin: "IE00B4L5Y983".into(),
                    quantity: d("2"),
                    avg_cost: None,
                },
            ],
            cash: Some(d("10")),
        };
        let issues = reconcile(
            &ledger(),
            Account::Regular,
            "2026-01-06".parse().unwrap(),
            &snapshot,
        );
        let kinds: Vec<_> = issues
            .iter()
            .map(|i| (i.isin.as_deref(), i.issue.as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (Some("US0378331005"), "quantity_mismatch"),
                (Some("IE00B4L5Y983"), "missing_in_ledger"),
            ]
        );
    }

    #[test]
    fn flags_negative_holdings_and_cash() {
        let mut postings = ledger().postings().to_vec();
        postings.push(Posting {
            event_id: "sell".into(),
            kind: Kind::Sell,
            isin: Some("US0378331005".into()),
            quantity: Some(d("-2")),
            amount: d("-500"),
            ..postings[1].clone()
        });
        let l = Ledger::new(postings, vec![], BTreeMap::new());
        let issues = consistency(&l, Account::Regular, "2026-01-06".parse().unwrap());
        let kinds: Vec<_> = issues.iter().map(|i| i.issue.as_str()).collect();
        assert_eq!(kinds, vec!["negative_quantity", "negative_cash"]);
    }

    #[test]
    fn matching_snapshot_has_no_issues() {
        let snapshot = PortfolioSnapshot {
            positions: vec![
                SnapshotPosition {
                    isin: "US0378331005".into(),
                    quantity: d("1.5"),
                    avg_cost: None,
                },
                SnapshotPosition {
                    isin: "DE000XS00019".into(),
                    quantity: d("3"),
                    avg_cost: None,
                },
            ],
            cash: Some(d("10")),
        };
        assert!(
            reconcile(
                &ledger(),
                Account::Regular,
                "2026-01-06".parse().unwrap(),
                &snapshot
            )
            .is_empty()
        );
    }
}
