use std::collections::{BTreeMap, BTreeSet};

use chrono::{Datelike, Days, NaiveDate, Weekday};
use rust_decimal::Decimal;
use serde::Serialize;

use crate::db::PriceRow;
use crate::ledger::{Account, Kind, Posting, Status};

pub const MAX_STALE_TRADING_DAYS: u32 = 3;
const NEGATIVE_CASH_TOLERANCE: Decimal = Decimal::from_parts(1, 0, 0, false, 2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    All,
    Account(Account),
}

impl Scope {
    fn includes(self, account: Account) -> bool {
        match self {
            Scope::All => true,
            Scope::Account(a) => a == account,
        }
    }

    fn is_flow(self, kind: Kind) -> bool {
        match self {
            Scope::All => kind.is_external_flow(),
            Scope::Account(_) => kind.is_external_flow() || kind == Kind::InternalTransfer,
        }
    }
}

pub struct Ledger {
    postings: Vec<Posting>,
    prices: BTreeMap<String, BTreeMap<NaiveDate, Decimal>>,
    names: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PricePoint {
    pub price: Decimal,
    pub date: NaiveDate,
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PositionValue {
    pub account: Account,
    pub isin: String,
    pub name: Option<String>,
    pub quantity: Decimal,
    pub price: Option<PricePoint>,
    pub value: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Valuation {
    pub date: NaiveDate,
    pub positions: Vec<PositionValue>,
    pub securities: Decimal,
    pub cash: Decimal,
    pub total: Decimal,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PeriodChange {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub start_value: Option<Decimal>,
    pub end_value: Option<Decimal>,
    pub net_flows: Decimal,
    pub change: Option<Decimal>,
    pub change_pct: Option<Decimal>,
    pub dividends: Decimal,
    pub interest: Decimal,
    pub fees: Decimal,
    pub taxes: Decimal,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mover {
    pub account: Account,
    pub isin: String,
    pub name: Option<String>,
    pub start_quantity: Decimal,
    pub end_quantity: Decimal,
    pub start_price: Option<Decimal>,
    pub end_price: Option<Decimal>,
    pub change: Option<Decimal>,
}

fn trading_days_between(from: NaiveDate, to: NaiveDate) -> u32 {
    from.iter_days()
        .skip(1)
        .take_while(|d| *d <= to)
        .filter(|d| !matches!(d.weekday(), Weekday::Sat | Weekday::Sun))
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

impl Ledger {
    pub fn new(
        postings: Vec<Posting>,
        prices: Vec<PriceRow>,
        names: BTreeMap<String, String>,
    ) -> Ledger {
        let mut by_isin: BTreeMap<String, BTreeMap<NaiveDate, Decimal>> = BTreeMap::new();
        for p in prices {
            by_isin.entry(p.isin).or_default().insert(p.date, p.price);
        }
        let mut postings: Vec<Posting> = postings
            .into_iter()
            .filter(|p| p.status == Status::Active)
            .collect();
        postings.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.ts.cmp(&b.ts)));
        Ledger {
            postings,
            prices: by_isin,
            names,
        }
    }

    pub fn postings(&self) -> &[Posting] {
        &self.postings
    }

    pub fn latest_price_date(&self) -> Option<NaiveDate> {
        self.prices
            .values()
            .filter_map(|m| m.keys().next_back().copied())
            .max()
    }

    pub fn name(&self, isin: &str) -> Option<String> {
        self.names.get(isin).cloned()
    }

    pub fn price_on(&self, isin: &str, date: NaiveDate) -> Option<PricePoint> {
        let (&price_date, &price) = self.prices.get(isin)?.range(..=date).next_back()?;
        (trading_days_between(price_date, date) <= MAX_STALE_TRADING_DAYS).then_some(PricePoint {
            price,
            date: price_date,
            stale: price_date < date,
        })
    }

    pub fn holdings(&self, scope: Scope, date: NaiveDate) -> BTreeMap<(Account, String), Decimal> {
        let mut holdings: BTreeMap<(Account, String), Decimal> = BTreeMap::new();
        for p in self
            .postings
            .iter()
            .filter(|p| p.date <= date && scope.includes(p.account))
        {
            if let (Some(isin), Some(qty)) = (&p.isin, p.quantity) {
                *holdings.entry((p.account, isin.clone())).or_default() += qty;
            }
        }
        holdings.retain(|_, qty| !qty.is_zero());
        holdings
    }

    pub fn held_isins(&self, date: NaiveDate) -> Vec<String> {
        self.holdings(Scope::All, date)
            .into_iter()
            .filter(|(_, qty)| *qty > Decimal::ZERO)
            .map(|((_, isin), _)| isin)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn cash(&self, scope: Scope, date: NaiveDate) -> Decimal {
        self.postings
            .iter()
            .filter(|p| p.date <= date && scope.includes(p.account))
            .map(|p| p.amount)
            .sum()
    }

    pub fn value_at(&self, scope: Scope, date: NaiveDate) -> Valuation {
        let positions: Vec<PositionValue> = self
            .holdings(scope, date)
            .into_iter()
            .map(|((account, isin), quantity)| {
                let price = self.price_on(&isin, date);
                PositionValue {
                    account,
                    name: self.name(&isin),
                    value: price.map(|p| (p.price * quantity).round_dp(2)),
                    isin,
                    quantity,
                    price,
                }
            })
            .collect();
        let securities: Decimal = positions.iter().filter_map(|p| p.value).sum();
        let cash = self.cash(scope, date);
        let cash_plausible = Account::ALL
            .into_iter()
            .filter(|a| scope.includes(*a))
            .all(|a| self.cash(Scope::Account(a), date) >= -NEGATIVE_CASH_TOLERANCE);
        Valuation {
            date,
            complete: cash_plausible
                && positions
                    .iter()
                    .all(|p| p.value.is_some() && p.quantity > Decimal::ZERO),
            positions,
            securities,
            cash,
            total: securities + cash,
        }
    }

    fn window(
        &self,
        scope: Scope,
        start: NaiveDate,
        end: NaiveDate,
    ) -> impl Iterator<Item = &Posting> {
        self.postings
            .iter()
            .filter(move |p| p.date > start && p.date <= end && scope.includes(p.account))
    }

    /// Change in value over `(start, end]`, net of money moved in or out.
    pub fn period_change(&self, scope: Scope, start: NaiveDate, end: NaiveDate) -> PeriodChange {
        let start_val = self.value_at(scope, start);
        let end_val = self.value_at(scope, end);
        let unpriced_moves = self
            .window(scope, start, end)
            .any(|p| p.quantity.is_some() && p.kind.moves_unpriced_securities());
        let complete = start_val.complete && end_val.complete && !unpriced_moves;

        let mut net_flows = Decimal::ZERO;
        let mut dividends = Decimal::ZERO;
        let mut interest = Decimal::ZERO;
        let mut fees = Decimal::ZERO;
        let mut taxes = Decimal::ZERO;
        for p in self.window(scope, start, end) {
            if scope.is_flow(p.kind) {
                net_flows += p.amount;
            }
            match p.kind {
                Kind::Dividend => dividends += p.amount,
                Kind::Interest => interest += p.amount,
                _ => {}
            }
            fees += p.fee;
            taxes += p.tax;
        }

        let change = complete.then(|| end_val.total - start_val.total - net_flows);
        let change_pct = change.and_then(|c| {
            (start_val.total > Decimal::ZERO)
                .then(|| (c / start_val.total * Decimal::ONE_HUNDRED).round_dp(2))
        });
        PeriodChange {
            start,
            end,
            start_value: complete.then_some(start_val.total),
            end_value: complete.then_some(end_val.total),
            net_flows,
            change,
            change_pct,
            dividends,
            interest,
            fees,
            taxes,
            complete,
        }
    }

    /// Per-holding price effect over `(start, end]`. Trades and rewards in the window count at
    /// their execution price, so buying more of something is not a gain.
    pub fn movers(&self, scope: Scope, start: NaiveDate, end: NaiveDate) -> Vec<Mover> {
        let start_holdings = self.holdings(scope, start);
        let end_holdings = self.holdings(scope, end);
        let mut keys: BTreeSet<(Account, String)> = start_holdings
            .keys()
            .chain(end_holdings.keys())
            .cloned()
            .collect();

        let mut traded: BTreeMap<(Account, String), Option<Decimal>> = BTreeMap::new();
        for p in self.window(scope, start, end) {
            let (Some(isin), Some(qty)) = (&p.isin, p.quantity) else {
                continue;
            };
            let key = (p.account, isin.clone());
            keys.insert(key.clone());
            let entry = traded.entry(key).or_insert(Some(Decimal::ZERO));
            *entry = match (*entry, p.price) {
                (Some(sum), Some(price)) => Some(sum + qty * price),
                _ => None,
            };
        }

        let mut movers: Vec<Mover> = keys
            .into_iter()
            .map(|(account, isin)| {
                let start_quantity = start_holdings
                    .get(&(account, isin.clone()))
                    .copied()
                    .unwrap_or_default();
                let end_quantity = end_holdings
                    .get(&(account, isin.clone()))
                    .copied()
                    .unwrap_or_default();
                let start_price = self.price_on(&isin, start).map(|p| p.price);
                let end_price = self.price_on(&isin, end).map(|p| p.price);
                let value = |qty: Decimal, price: Option<Decimal>| {
                    if qty.is_zero() {
                        Some(Decimal::ZERO)
                    } else {
                        price.map(|p| qty * p)
                    }
                };
                let invested = traded
                    .get(&(account, isin.clone()))
                    .copied()
                    .unwrap_or(Some(Decimal::ZERO));
                let change = match (
                    value(end_quantity, end_price),
                    value(start_quantity, start_price),
                    invested,
                ) {
                    (Some(e), Some(s), Some(i)) => Some((e - s - i).round_dp(2)),
                    _ => None,
                };
                Mover {
                    account,
                    name: self.name(&isin),
                    isin,
                    start_quantity,
                    end_quantity,
                    start_price,
                    end_price,
                    change,
                }
            })
            .collect();
        movers.sort_by(|a, b| b.change.cmp(&a.change).then_with(|| a.isin.cmp(&b.isin)));
        movers
    }

    pub fn period_start(end: NaiveDate, days: u64) -> NaiveDate {
        end.checked_sub_days(Days::new(days))
            .unwrap_or(NaiveDate::MIN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::d;

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn posting(
        day: &str,
        account: Account,
        kind: Kind,
        isin: Option<&str>,
        qty: Option<&str>,
        price: Option<&str>,
        amount: &str,
    ) -> Posting {
        Posting {
            event_id: format!("{day}-{}", kind.as_str()),
            leg: 0,
            account,
            ts: format!("{day}T10:00:00.000+0000"),
            date: date(day),
            isin: isin.map(str::to_owned),
            instrument_name: None,
            kind,
            quantity: qty.map(d),
            price: price.map(d),
            amount: d(amount),
            fee: Decimal::ZERO,
            tax: Decimal::ZERO,
            saving_plan: false,
            status: Status::Active,
        }
    }

    fn price(isin: &str, day: &str, value: &str) -> PriceRow {
        PriceRow {
            isin: isin.into(),
            date: date(day),
            price: d(value),
            currency: "EUR".into(),
            basis: "last".into(),
            observed_at: format!("{day}T21:15:00Z"),
            venue: None,
            source: "test".into(),
        }
    }

    const A: &str = "US0378331005";

    fn ledger() -> Ledger {
        Ledger::new(
            vec![
                posting(
                    "2026-01-05",
                    Account::Regular,
                    Kind::Deposit,
                    None,
                    None,
                    None,
                    "1000",
                ),
                posting(
                    "2026-01-05",
                    Account::Regular,
                    Kind::Buy,
                    Some(A),
                    Some("10"),
                    Some("50"),
                    "-501",
                ),
                posting(
                    "2026-01-07",
                    Account::Regular,
                    Kind::Deposit,
                    None,
                    None,
                    None,
                    "100",
                ),
                posting(
                    "2026-01-08",
                    Account::Regular,
                    Kind::Buy,
                    Some(A),
                    Some("2"),
                    Some("55"),
                    "-110",
                ),
                posting(
                    "2026-01-08",
                    Account::Regular,
                    Kind::Dividend,
                    Some(A),
                    None,
                    None,
                    "3",
                ),
                posting(
                    "2026-01-09",
                    Account::Regular,
                    Kind::InternalTransfer,
                    None,
                    None,
                    None,
                    "-50",
                ),
                posting(
                    "2026-01-09",
                    Account::Pea,
                    Kind::InternalTransfer,
                    None,
                    None,
                    None,
                    "50",
                ),
            ],
            vec![price(A, "2026-01-05", "50"), price(A, "2026-01-12", "60")],
            BTreeMap::new(),
        )
    }

    #[test]
    fn values_holdings_and_cash() {
        let v = ledger().value_at(Scope::All, date("2026-01-12"));
        assert_eq!(v.securities, d("720"));
        assert_eq!(v.cash, d("1000") - d("501") + d("100") - d("110") + d("3"));
        assert!(v.complete);
    }

    #[test]
    fn period_change_excludes_deposits_and_internal_transfers() {
        let change = ledger().period_change(Scope::All, date("2026-01-05"), date("2026-01-12"));
        assert_eq!(change.net_flows, d("100"));
        // start: 10 × 50 + 499 cash = 999; end: 12 × 60 + 492 cash = 1212.
        assert_eq!(change.change, Some(d("1212") - d("999") - d("100")));
        assert_eq!(change.dividends, d("3"));
    }

    #[test]
    fn corrections_count_as_flows_and_unpriced_moves_mark_incomplete() {
        let mut postings = ledger().postings().to_vec();
        postings.push(posting(
            "2026-01-08",
            Account::Regular,
            Kind::Adjustment,
            None,
            None,
            None,
            "40",
        ));
        let l = Ledger::new(
            postings.clone(),
            vec![price(A, "2026-01-05", "50"), price(A, "2026-01-12", "60")],
            BTreeMap::new(),
        );
        let change = l.period_change(Scope::All, date("2026-01-05"), date("2026-01-12"));
        assert_eq!(change.net_flows, d("140"));
        assert_eq!(change.change, Some(d("1252") - d("999") - d("140")));

        postings.push(posting(
            "2026-01-08",
            Account::Regular,
            Kind::OpeningBalance,
            Some(A),
            Some("1"),
            None,
            "0",
        ));
        let l = Ledger::new(
            postings,
            vec![price(A, "2026-01-05", "50"), price(A, "2026-01-12", "60")],
            BTreeMap::new(),
        );
        assert!(
            !l.period_change(Scope::All, date("2026-01-05"), date("2026-01-12"))
                .complete
        );
    }

    #[test]
    fn internal_transfer_is_a_flow_for_one_account() {
        let change = ledger().period_change(
            Scope::Account(Account::Pea),
            date("2026-01-05"),
            date("2026-01-12"),
        );
        assert_eq!(change.net_flows, d("50"));
        assert_eq!(change.change, Some(Decimal::ZERO));
    }

    #[test]
    fn stale_prices_expire_after_three_trading_days() {
        let l = ledger();
        assert_eq!(
            l.price_on(A, date("2026-01-08")).map(|p| p.stale),
            Some(true)
        );
        assert_eq!(l.price_on(A, date("2026-01-09")), None);
        let change = l.period_change(Scope::All, date("2026-01-05"), date("2026-01-09"));
        assert!(!change.complete);
        assert_eq!(change.change, None);
    }

    #[test]
    fn movers_count_buys_at_execution_price() {
        let movers = ledger().movers(Scope::All, date("2026-01-05"), date("2026-01-12"));
        assert_eq!(movers.len(), 1);
        // 12 × 60 − 10 × 50 − 2 × 55 = 110
        assert_eq!(movers[0].change, Some(d("110")));
    }
}
