use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use chrono::NaiveDate;
use rust_decimal::Decimal;

use crate::error::Error;
use crate::event::RawEvent;
use crate::numbers::{Currency, parse_amount};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Account {
    Regular,
    Pea,
}

impl Account {
    pub const ALL: [Account; 2] = [Account::Regular, Account::Pea];

    pub fn as_str(self) -> &'static str {
        match self {
            Account::Regular => "regular",
            Account::Pea => "pea",
        }
    }
}

impl fmt::Display for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Account {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "regular" => Ok(Account::Regular),
            "pea" => Ok(Account::Pea),
            other => Err(format!("unknown account {other:?}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Buy,
    Sell,
    Dividend,
    Interest,
    Deposit,
    Withdrawal,
    Fee,
    Tax,
    Reward,
    Split,
    CorporateAction,
    TransferIn,
    TransferOut,
    InternalTransfer,
    OpeningBalance,
    Adjustment,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Buy => "buy",
            Kind::Sell => "sell",
            Kind::Dividend => "dividend",
            Kind::Interest => "interest",
            Kind::Deposit => "deposit",
            Kind::Withdrawal => "withdrawal",
            Kind::Fee => "fee",
            Kind::Tax => "tax",
            Kind::Reward => "reward",
            Kind::Split => "split",
            Kind::CorporateAction => "corporate_action",
            Kind::TransferIn => "transfer_in",
            Kind::TransferOut => "transfer_out",
            Kind::InternalTransfer => "internal_transfer",
            Kind::OpeningBalance => "opening_balance",
            Kind::Adjustment => "adjustment",
        }
    }

    /// Cash that enters or leaves without being earned: deposits, withdrawals, and corrections
    /// for money the timeline doesn't show.
    pub fn is_external_flow(self) -> bool {
        matches!(
            self,
            Kind::Deposit | Kind::Withdrawal | Kind::OpeningBalance | Kind::Adjustment
        )
    }

    /// Kinds that change holdings with no execution price, so their value effect is unknown.
    pub fn moves_unpriced_securities(self) -> bool {
        matches!(
            self,
            Kind::OpeningBalance
                | Kind::Adjustment
                | Kind::TransferIn
                | Kind::TransferOut
                | Kind::CorporateAction
        )
    }
}

impl FromStr for Kind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        [
            Kind::Buy,
            Kind::Sell,
            Kind::Dividend,
            Kind::Interest,
            Kind::Deposit,
            Kind::Withdrawal,
            Kind::Fee,
            Kind::Tax,
            Kind::Reward,
            Kind::Split,
            Kind::CorporateAction,
            Kind::TransferIn,
            Kind::TransferOut,
            Kind::InternalTransfer,
            Kind::OpeningBalance,
            Kind::Adjustment,
        ]
        .into_iter()
        .find(|k| k.as_str() == s)
        .ok_or_else(|| format!("unknown transaction kind {s:?}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Active,
    Cancelled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Posting {
    pub event_id: String,
    pub leg: i64,
    pub account: Account,
    pub ts: String,
    pub date: NaiveDate,
    pub isin: Option<String>,
    pub instrument_name: Option<String>,
    pub kind: Kind,
    /// Signed change in quantity held: positive for buys and rewards, negative for sells.
    pub quantity: Option<Decimal>,
    pub price: Option<Decimal>,
    /// Signed effect on cash, fees and taxes included.
    pub amount: Decimal,
    pub fee: Decimal,
    pub tax: Decimal,
    pub saving_plan: bool,
    pub status: Status,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountMap {
    pub pea_cash_account: Option<String>,
    pub regular_cash_account: Option<String>,
}

impl AccountMap {
    /// The PEA is the cash account whose trades show `Portfolio: PEA`. Any other cash account is
    /// the regular one; more than one of those is an error rather than a guess.
    pub fn detect<'a>(
        events: impl IntoIterator<Item = &'a RawEvent>,
        known: &AccountMap,
    ) -> Result<AccountMap, Error> {
        let mut pea = BTreeSet::new();
        let mut all = BTreeSet::new();
        for e in events {
            let Some(cash) = e.cash_account_number.as_deref() else {
                continue;
            };
            all.insert(cash.to_owned());
            if TRADE_TYPES.contains(&e.event_type())
                && e.row_text(&[], &["Portfolio"]) == Some("PEA")
            {
                pea.insert(cash.to_owned());
            }
        }
        pea.extend(known.pea_cash_account.clone());
        let regular: BTreeSet<_> = all
            .difference(&pea)
            .cloned()
            .chain(known.regular_cash_account.clone())
            .collect();

        if pea.len() > 1 || regular.len() > 1 || !pea.is_disjoint(&regular) {
            return Err(Error::Accounts(format!(
                "expected one regular and at most one PEA cash account, found {} PEA and {} other",
                pea.len(),
                regular.len()
            )));
        }
        Ok(AccountMap {
            pea_cash_account: pea.into_iter().next(),
            regular_cash_account: regular.into_iter().next(),
        })
    }

    pub fn account_of(&self, event: &RawEvent) -> Account {
        match (&event.cash_account_number, &self.pea_cash_account) {
            (Some(cash), Some(pea)) if cash == pea => Account::Pea,
            _ => Account::Regular,
        }
    }
}

const TRADE_TYPES: &[&str] = &["TRADING_TRADE_EXECUTED", "TRADE_INVOICE"];
const DEPOSIT_TYPES: &[&str] = &[
    "BANK_TRANSACTION_INCOMING",
    "PAYMENT_INBOUND",
    "PAYMENT_INBOUND_APPLE_PAY",
];
const REWARD_TYPES: &[&str] = &["ACQUISITION_TRADE_PERK", "STOCK_PERK_REFUNDED"];
/// Meeting invitations and ISIN switches ("Wechsel") arrive with these types too, but carry no
/// share movement in their details; only events with a "shares added" row produce postings.
const CORPORATE_ACTION_TYPES: &[&str] = &[
    "SSP_CORPORATE_ACTION_ACTIVITY",
    "SSP_CORPORATE_ACTION_NO_CASH",
];
const SHARES_ADDED: &[&str] = &["Aktien hinzugefügt", "Shares added"];

/// Event types that never move cash or securities.
pub const IGNORED_TYPES: &[&str] = &[
    "ORDER_REJECTED",
    "ORDER_CANCELLED",
    "ORDER_EXPIRED",
    "TRADING_ORDER_CANCELLED",
    "TRADING_ORDER_REJECTED",
    "TRADING_ORDER_EXPIRED",
    "TRADING_ORDER_CREATED",
    "SSP_CORPORATE_ACTION_INFORMATIVE",
    "SSP_CORPORATE_ACTION_UPCOMING",
    "DOCUMENTS_ACCEPTED",
    "DOCUMENTS_CREATED",
    "CSX_CHAT_ACTIVITY",
    "TAX_YEAR_END_REPORT_CREATED",
    "EX_POST_COST_REPORT_CREATED",
    "QUARTERLY_NET_WORTH_STATEMENT_CREATED",
    "CUSTOMER_CREATED",
    "SECURITIES_ACCOUNT_CREATED",
    "EMAIL_VALIDATED",
    "PUK_CREATED",
    "REFERENCE_ACCOUNT_CHANGED",
    "INTEREST_PAYOUT_CREATED",
    "CRYPTO_TNC_UPDATE_2025",
    "PRIVATE_MARKETS_SUITABILITY_QUIZ_COMPLETED",
    "TAX_CUSTOMER_CONFIGURATION_CHANGED",
    "IPO_SUBSCRIPTION_CANCELLED",
    "",
];

const OVERVIEW: &[&str] = &["Übersicht", "Overview"];
const TRANSACTION: &[&str] = &["Transaktion", "Transaction"];
const FEE: &[&str] = &["Gebühr", "Fee"];
const TAX: &[&str] = &["Steuer", "Steuern", "Tax", "Taxes"];
const ASSET: &[&str] = &["Asset", "Vermögenswert", "Wertpapier", "Produkt"];

pub fn postings_for(event: &RawEvent, accounts: &AccountMap) -> Result<Vec<Posting>, Error> {
    let event_type = event.event_type();
    if IGNORED_TYPES.contains(&event_type) {
        return Ok(Vec::new());
    }

    let p = Parser {
        event,
        account: accounts.account_of(event),
    };
    let status = match event.status.as_deref() {
        Some("CANCELED" | "CANCELLED") => Status::Cancelled,
        _ => Status::Active,
    };

    let mut postings = match event_type {
        t if TRADE_TYPES.contains(&t) => vec![p.trade()?],
        "PEA_DEPOSIT_DEBIT" => p.pea_deposit()?,
        "SSP_CORPORATE_ACTION_CASH" => vec![p.dividend()?],
        "INTEREST_PAYOUT" => {
            vec![p.cash(Kind::Interest, p.amount()?, p.optional_magnitude(&[], TAX)?)?]
        }
        t if DEPOSIT_TYPES.contains(&t) => p.deposit()?,
        "PAYMENT_OUTBOUND" => vec![p.cash(Kind::Withdrawal, p.amount()?, Decimal::ZERO)?],
        "SSP_SECURITIES_TRANSFER_INCOMING" => p.transfer_in()?,
        "GIFTING_RECIPIENT_ACTIVITY" => p.gift()?,
        t if REWARD_TYPES.contains(&t) => vec![p.reward_shares()?],
        t if CORPORATE_ACTION_TYPES.contains(&t) => p.corporate_action_shares()?,
        _ => {
            return Err(Error::UnknownEvent {
                event_id: event.id.clone(),
                event_type: event_type.to_owned(),
                title: event.title.clone(),
            });
        }
    };

    for (leg, posting) in postings.iter_mut().enumerate() {
        posting.leg = i64::try_from(leg).unwrap_or(i64::MAX);
        if status == Status::Cancelled {
            posting.status = Status::Cancelled;
        }
    }
    Ok(postings)
}

fn normalized_name(name: &str) -> String {
    const SUFFIXES: &[&str] = &[
        "inc",
        "corp",
        "corporation",
        "ag",
        "sa",
        "se",
        "plc",
        "nv",
        "co",
        "ltd",
    ];
    name.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !SUFFIXES.contains(w))
        .collect::<Vec<_>>()
        .join(" ")
}

fn same_company(a: &str, b: &str) -> bool {
    let a = normalized_name(a);
    !a.is_empty() && a == normalized_name(b)
}

/// Corporate actions carry no cash account and, for spin-offs, no ISIN for the new security.
/// This fills both in from the rest of the history: the ISIN by matching the security's name
/// against other postings, the account from whichever account held the parent at the time.
pub fn resolve_corporate_actions(events: &[RawEvent], postings: &mut [Vec<Posting>]) {
    let mut known: Vec<(String, String)> = postings
        .iter()
        .flatten()
        .filter(|p| p.kind != Kind::CorporateAction)
        .filter_map(|p| Some((p.instrument_name.clone()?, p.isin.clone()?)))
        .collect();
    known.sort();
    known.dedup();

    let mut order: Vec<usize> = (0..events.len()).collect();
    order.sort_by(|&a, &b| events[a].timestamp().cmp(&events[b].timestamp()));

    let mut held: std::collections::HashMap<(Account, String), Decimal> =
        std::collections::HashMap::new();
    for i in order {
        let parent = events[i].isin();
        for p in postings[i].iter_mut() {
            if p.kind == Kind::CorporateAction && events[i].cash_account_number.is_none() {
                if p.isin.is_none()
                    && let Some(name) = &p.instrument_name
                {
                    p.isin = known
                        .iter()
                        .find(|(n, _)| same_company(n, name))
                        .map(|(_, isin)| isin.clone());
                }
                if let Some(parent) = &parent {
                    let holders: Vec<Account> = Account::ALL
                        .into_iter()
                        .filter(|a| {
                            held.get(&(*a, parent.clone()))
                                .is_some_and(|q| *q > Decimal::ZERO)
                        })
                        .collect();
                    if let [only] = holders.as_slice() {
                        p.account = *only;
                    }
                }
            }
            if let (Some(isin), Some(qty)) = (&p.isin, p.quantity)
                && p.status == Status::Active
            {
                *held.entry((p.account, isin.clone())).or_default() += qty;
            }
        }
    }
}

struct Parser<'a> {
    event: &'a RawEvent,
    account: Account,
}

impl Parser<'_> {
    fn fail(&self, reason: impl Into<String>) -> Error {
        Error::Parse {
            event_id: self.event.id.clone(),
            event_type: self.event.event_type().to_owned(),
            reason: reason.into(),
        }
    }

    fn base(&self, kind: Kind, amount: Decimal) -> Result<Posting, Error> {
        Ok(Posting {
            event_id: self.event.id.clone(),
            leg: 0,
            account: self.account,
            ts: self.event.timestamp.clone(),
            date: self
                .event
                .local_date()
                .ok_or_else(|| self.fail("unreadable timestamp"))?,
            isin: None,
            instrument_name: None,
            kind,
            quantity: None,
            price: None,
            amount,
            fee: Decimal::ZERO,
            tax: Decimal::ZERO,
            saving_plan: false,
            status: Status::Active,
        })
    }

    fn cash(&self, kind: Kind, amount: Decimal, tax: Decimal) -> Result<Posting, Error> {
        let mut posting = self.base(kind, amount)?;
        posting.tax = tax;
        Ok(posting)
    }

    fn amount(&self) -> Result<Decimal, Error> {
        self.event
            .amount_eur()
            .ok_or_else(|| self.fail("missing EUR amount"))
    }

    fn isin(&self) -> Result<String, Error> {
        self.event.isin().ok_or_else(|| self.fail("no ISIN found"))
    }

    fn instrument_name(&self) -> String {
        self.event
            .row_text(OVERVIEW, ASSET)
            .map(str::to_owned)
            .unwrap_or_else(|| self.event.title.clone())
    }

    fn decimal(&self, sections: &[&str], labels: &[&str]) -> Result<Decimal, Error> {
        self.event
            .row_amount(sections, labels)
            .map(|a| a.value)
            .ok_or_else(|| self.fail(format!("missing or unreadable row {labels:?}")))
    }

    fn optional_magnitude(&self, sections: &[&str], labels: &[&str]) -> Result<Decimal, Error> {
        match self.event.row_text(sections, labels) {
            None => Ok(Decimal::ZERO),
            Some(text) => parse_amount(text)
                .map(|a| a.value.abs())
                .ok_or_else(|| self.fail(format!("unreadable row {labels:?}: {text:?}"))),
        }
    }

    fn eur_price(&self, text: &str) -> Result<Decimal, Error> {
        let price =
            parse_amount(text).ok_or_else(|| self.fail(format!("unreadable price {text:?}")))?;
        match price.currency {
            Some(Currency::Eur) | None => Ok(price.value),
            Some(other) => Err(self.fail(format!("price in {other:?}, only EUR is supported"))),
        }
    }

    /// Quantity and unit price, from either `0,25 × 120,00 €` or separate rows.
    fn quantity_and_price(&self) -> Result<(Decimal, Decimal), Error> {
        if let Some(text) = self
            .event
            .row_text(OVERVIEW, &["Transaktion", "Transaction"])
        {
            let (qty, price) = text
                .split_once('×')
                .ok_or_else(|| self.fail(format!("unreadable transaction row {text:?}")))?;
            let qty = parse_amount(qty)
                .ok_or_else(|| self.fail(format!("unreadable quantity {qty:?}")))?;
            return Ok((qty.value, self.eur_price(price)?));
        }
        let qty = self.decimal(&[], &["Anteile", "Aktien", "Shares"])?;
        let price_text = self
            .event
            .row_text(&[], &["Aktienkurs", "Kurs", "Price"])
            .ok_or_else(|| self.fail("missing price row"))?;
        Ok((qty, self.eur_price(price_text)?))
    }

    fn instrument_posting(&self, kind: Kind, amount: Decimal) -> Result<Posting, Error> {
        let mut posting = self.base(kind, amount)?;
        posting.isin = Some(self.isin()?);
        posting.instrument_name = Some(self.instrument_name());
        Ok(posting)
    }

    /// The order label decides the side. The amount's sign can't: selling a tiny fraction can
    /// cost more in fees than it brings in.
    fn trade_side(&self, amount: Decimal) -> Kind {
        let labels = [
            self.event.subtitle.as_deref(),
            self.event
                .row_text(OVERVIEW, &["Orderart", "Ordertyp", "Order type"]),
        ];
        for label in labels.into_iter().flatten().map(str::to_lowercase) {
            if label.contains("verkauf") || label.contains("sell") {
                return Kind::Sell;
            }
            if label.contains("kauf") || label.contains("buy") {
                return Kind::Buy;
            }
        }
        if amount < Decimal::ZERO {
            Kind::Buy
        } else {
            Kind::Sell
        }
    }

    fn trade(&self) -> Result<Posting, Error> {
        let amount = self.amount()?;
        let (qty, price) = self.quantity_and_price()?;
        let kind = self.trade_side(amount);
        let mut posting = self.instrument_posting(kind, amount)?;
        posting.quantity = Some(if kind == Kind::Buy {
            qty.abs()
        } else {
            -qty.abs()
        });
        posting.price = Some(price);
        posting.fee = self.optional_magnitude(&[], FEE)?;
        posting.tax = self.optional_magnitude(&[], TAX)?;
        Ok(posting)
    }

    fn pea_deposit(&self) -> Result<Vec<Posting>, Error> {
        let amount = self.amount()?;
        let mut debit = self.base(Kind::InternalTransfer, amount)?;
        debit.account = Account::Regular;
        let mut credit = self.base(Kind::InternalTransfer, -amount)?;
        credit.account = Account::Pea;
        Ok(vec![debit, credit])
    }

    fn dividend(&self) -> Result<Posting, Error> {
        let mut posting = self.instrument_posting(Kind::Dividend, self.amount()?)?;
        posting.tax = self.optional_magnitude(&[], TAX)?;
        Ok(posting)
    }

    /// Card and Apple Pay deposits show the gross amount; the fee is charged on top as its own leg.
    fn deposit(&self) -> Result<Vec<Posting>, Error> {
        let mut postings = vec![self.cash(Kind::Deposit, self.amount()?, Decimal::ZERO)?];
        let fee = self.optional_magnitude(TRANSACTION, FEE)?;
        if fee > Decimal::ZERO {
            let mut fee_leg = self.cash(Kind::Fee, -fee, Decimal::ZERO)?;
            fee_leg.fee = fee;
            postings.push(fee_leg);
        }
        Ok(postings)
    }

    fn transfer_in(&self) -> Result<Vec<Posting>, Error> {
        let qty = self.decimal(TRANSACTION, &["Shares", "Aktien", "Anteile"])?;
        if qty.is_zero() {
            return Ok(Vec::new());
        }
        let mut posting = self.instrument_posting(Kind::TransferIn, Decimal::ZERO)?;
        posting.quantity = Some(qty);
        Ok(vec![posting])
    }

    fn gift(&self) -> Result<Vec<Posting>, Error> {
        let qty = self.decimal(OVERVIEW, &["Aktien", "Anteile", "Shares"])?;
        let price_text = self
            .event
            .row_text(OVERVIEW, &["Aktienkurs", "Price"])
            .ok_or_else(|| self.fail("missing price row"))?;
        let mut shares = self.instrument_posting(Kind::Reward, Decimal::ZERO)?;
        shares.quantity = Some(qty);
        shares.price = Some(self.eur_price(price_text)?);

        let mut postings = vec![shares];
        let cash_credit =
            self.optional_magnitude(TRANSACTION, &["Cash-Gutschrift", "Cash credit"])?;
        if cash_credit > Decimal::ZERO {
            postings.push(self.base(Kind::Reward, cash_credit)?);
        }
        Ok(postings)
    }

    /// Bonus shares name the event's own company and use its ISIN. A spin-off names the new
    /// company, whose ISIN is left for `resolve_corporate_actions` to find by name.
    fn corporate_action_shares(&self) -> Result<Vec<Posting>, Error> {
        let Some(added) = self.event.row_amount(OVERVIEW, SHARES_ADDED) else {
            return Ok(Vec::new());
        };
        let security = self
            .event
            .row_text(OVERVIEW, &["Wertpapier", "Security", "Asset"])
            .unwrap_or(&self.event.title)
            .to_owned();
        let mut posting = self.base(Kind::CorporateAction, Decimal::ZERO)?;
        posting.quantity = Some(added.value);
        if same_company(&security, &self.event.title) {
            posting.isin = Some(self.isin()?);
        }
        posting.instrument_name = Some(security);
        Ok(vec![posting])
    }

    fn reward_shares(&self) -> Result<Posting, Error> {
        let (qty, price) = self.quantity_and_price()?;
        let mut posting = self.instrument_posting(Kind::Reward, Decimal::ZERO)?;
        posting.quantity = Some(qty);
        posting.price = Some(price);
        Ok(posting)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testing::{d, event, table};

    fn accounts() -> AccountMap {
        AccountMap {
            pea_cash_account: Some("PEA-CASH".into()),
            regular_cash_account: Some("REG-CASH".into()),
        }
    }

    #[test]
    fn parses_current_buy_with_tax_and_fee() {
        let e = event(
            "TRADING_TRADE_EXECUTED",
            json!({
                "cashAccountNumber": "REG-CASH",
                "amount": {"value": -112.30, "currency": "EUR", "fractionDigits": 2},
                "icon": "logos/FR0000131104/v2",
                "details": {"sections": [table("Übersicht", &[
                    ("Kauf", "Ausgeführt"), ("Portfolio", "Brokerage"), ("Asset", "BNP Paribas"),
                    ("Transaktion", "2 ×  55,40\u{a0}€"), ("Steuer", "0,44\u{a0}€"),
                    ("Gebühr", "1,06\u{a0}€"), ("Summe", "112,30\u{a0}€"),
                ])]}
            }),
        );
        let postings = postings_for(&e, &accounts()).unwrap();
        assert_eq!(postings.len(), 1);
        let p = &postings[0];
        assert_eq!((p.kind, p.account), (Kind::Buy, Account::Regular));
        assert_eq!(p.isin.as_deref(), Some("FR0000131104"));
        assert_eq!(p.instrument_name.as_deref(), Some("BNP Paribas"));
        assert_eq!(p.quantity, Some(d("2")));
        assert_eq!(p.price, Some(d("55.40")));
        assert_eq!(
            (p.amount, p.fee, p.tax),
            (d("-112.30"), d("1.06"), d("0.44"))
        );
    }

    #[test]
    fn parses_sell_in_pea() {
        let e = event(
            "TRADING_TRADE_EXECUTED",
            json!({
                "cashAccountNumber": "PEA-CASH",
                "amount": {"value": 29.00, "currency": "EUR", "fractionDigits": 2},
                "icon": "logos/FR0000120321/v2",
                "details": {"sections": [table("Übersicht", &[
                    ("Verkaufen", "Ausgeführt"), ("Portfolio", "PEA"), ("Asset", "L'Oréal"),
                    ("Transaktion", "0,25 ×  120,00\u{a0}€"), ("Gebühr", "1,00\u{a0}€"),
                ])]}
            }),
        );
        let p = &postings_for(&e, &accounts()).unwrap()[0];
        assert_eq!((p.kind, p.account), (Kind::Sell, Account::Pea));
        assert_eq!(p.quantity, Some(d("-0.25")));
    }

    #[test]
    fn parses_old_trade_invoice() {
        let e = event(
            "TRADE_INVOICE",
            json!({
                "amount": {"value": -10.0, "currency": "EUR", "fractionDigits": 2},
                "icon": "logos/US0231351067/v2",
                "details": {"sections": [
                    table("Übersicht", &[("Portfolio", "Brokerage"), ("Orderart", "Kauf"), ("Asset", "Amazon.com")]),
                    table("Transaktion", &[("Anteile", "0,061234"), ("Aktienkurs", "150,00\u{a0}€"), ("Gebühr", "1,00\u{a0}€")]),
                ]}
            }),
        );
        let p = &postings_for(&e, &accounts()).unwrap()[0];
        assert_eq!((p.kind, p.account), (Kind::Buy, Account::Regular));
        assert_eq!(
            (p.quantity, p.price, p.fee),
            (Some(d("0.061234")), Some(d("150.00")), d("1.00"))
        );
    }

    #[test]
    fn splits_pea_deposit_into_two_legs() {
        let e = event(
            "PEA_DEPOSIT_DEBIT",
            json!({"cashAccountNumber": "REG-CASH", "amount": {"value": -42.50, "currency": "EUR", "fractionDigits": 2}}),
        );
        let postings = postings_for(&e, &accounts()).unwrap();
        let legs: Vec<_> = postings
            .iter()
            .map(|p| (p.leg, p.account, p.kind, p.amount))
            .collect();
        assert_eq!(
            legs,
            vec![
                (0, Account::Regular, Kind::InternalTransfer, d("-42.50")),
                (1, Account::Pea, Kind::InternalTransfer, d("42.50")),
            ]
        );
    }

    #[test]
    fn marks_cancelled_dividend() {
        let e = event(
            "SSP_CORPORATE_ACTION_CASH",
            json!({
                "status": "CANCELED",
                "amount": {"value": 0.12, "currency": "EUR", "fractionDigits": 2},
                "icon": "logos/US0378331005/v2",
                "details": {"sections": [table("Geschäft", &[("Aktien", "0.512345"), ("Steuer", "-0,02\u{a0}€")])]}
            }),
        );
        let p = &postings_for(&e, &accounts()).unwrap()[0];
        assert_eq!(
            (p.kind, p.status, p.tax),
            (Kind::Dividend, Status::Cancelled, d("0.02"))
        );
    }

    #[test]
    fn adds_fee_leg_for_card_deposit() {
        let e = event(
            "PAYMENT_INBOUND_APPLE_PAY",
            json!({
                "amount": {"value": 40.0, "currency": "EUR", "fractionDigits": 2},
                "details": {"sections": [table("Transaktion", &[("Gebühr", "0,28\u{a0}€"), ("Betrag", "40,00\u{a0}€")])]}
            }),
        );
        let legs: Vec<_> = postings_for(&e, &accounts())
            .unwrap()
            .iter()
            .map(|p| (p.kind, p.amount))
            .collect();
        assert_eq!(
            legs,
            vec![(Kind::Deposit, d("40")), (Kind::Fee, d("-0.28"))]
        );
    }

    #[test]
    fn parses_gift_with_cash_credit() {
        let e = event(
            "GIFTING_RECIPIENT_ACTIVITY",
            json!({
                "details": {"sections": [
                    table("Übersicht", &[("Asset", "BNP Paribas"), ("Aktien", "0.499000"), ("Aktienkurs", "100,00\u{a0}€")]),
                    table("Transaktion", &[("Aktien", "49,90\u{a0}€"), ("Cash-Gutschrift", "0,10\u{a0}€")]),
                    {"type": "header", "title": "x", "action": {"type": "instrumentDetail", "payload": "FR0000131104"}},
                ]}
            }),
        );
        let legs: Vec<_> = postings_for(&e, &accounts())
            .unwrap()
            .iter()
            .map(|p| (p.kind, p.quantity, p.amount))
            .collect();
        assert_eq!(
            legs,
            vec![
                (Kind::Reward, Some(d("0.499000")), d("0")),
                (Kind::Reward, None, d("0.10"))
            ]
        );
    }

    #[test]
    fn skips_zero_share_transfer() {
        let e = event(
            "SSP_SECURITIES_TRANSFER_INCOMING",
            json!({"icon": "logos/US02079K3059/v2", "details": {"sections": [table("Transaction", &[("Shares", "0"), ("Fee", "Free")])]}}),
        );
        assert!(postings_for(&e, &accounts()).unwrap().is_empty());
    }

    #[test]
    fn tiny_sell_with_negative_amount_is_still_a_sell() {
        let e = event(
            "TRADING_TRADE_EXECUTED",
            json!({
                "subtitle": "Verkaufsorder",
                "amount": {"value": -0.50, "currency": "EUR", "fractionDigits": 2},
                "icon": "logos/US0231351067/v2",
                "details": {"sections": [table("Übersicht", &[
                    ("Verkaufen", "Ausgeführt"), ("Transaktion", "0,01 ×  50,00\u{a0}€"), ("Gebühr", "1,00\u{a0}€"),
                ])]}
            }),
        );
        let p = &postings_for(&e, &accounts()).unwrap()[0];
        assert_eq!(
            (p.kind, p.quantity, p.amount),
            (Kind::Sell, Some(d("-0.01")), d("-0.50"))
        );
    }

    fn corporate_action(
        id: &str,
        ts: &str,
        title: &str,
        icon: &str,
        security: &str,
        added: &str,
    ) -> RawEvent {
        let mut e = event(
            "SSP_CORPORATE_ACTION_ACTIVITY",
            json!({
                "timestamp": ts,
                "title": title,
                "status": null,
                "icon": icon,
                "details": {"sections": [table("Übersicht", &[("Wertpapier", security), ("Aktien hinzugefügt", added)])]}
            }),
        );
        e.id = id.into();
        e
    }

    fn trade(
        id: &str,
        ts: &str,
        subtitle: &str,
        isin: &str,
        name: &str,
        qty: &str,
        amount: f64,
    ) -> RawEvent {
        let mut e = event(
            "TRADING_TRADE_EXECUTED",
            json!({
                "timestamp": ts,
                "subtitle": subtitle,
                "cashAccountNumber": "PEA-CASH",
                "amount": {"value": amount, "currency": "EUR", "fractionDigits": 2},
                "icon": format!("logos/{isin}/v2"),
                "details": {"sections": [table("Übersicht", &[("Asset", name), ("Transaktion", &format!("{qty} ×  1,00\u{a0}€"))])]}
            }),
        );
        e.id = id.into();
        e
    }

    #[test]
    fn resolves_spin_off_isin_and_account() {
        let events = vec![
            trade(
                "buy",
                "2025-01-01T10:00:00.000+0000",
                "Kauforder",
                "DE0007236101",
                "Siemens",
                "1",
                -20.0,
            ),
            corporate_action(
                "spin",
                "2025-02-01T10:00:00.000+0000",
                "Siemens",
                "logos/DE0007236101/v2",
                "SIEMENS ENERGY AG",
                "2.000000",
            ),
            trade(
                "sell",
                "2025-03-01T10:00:00.000+0000",
                "Verkaufsorder",
                "DE000ENER6Y0",
                "Siemens Energy",
                "2",
                4.0,
            ),
        ];
        let mut postings: Vec<Vec<Posting>> = events
            .iter()
            .map(|e| postings_for(e, &accounts()).unwrap())
            .collect();
        resolve_corporate_actions(&events, &mut postings);
        let spin = &postings[1][0];
        assert_eq!(spin.kind, Kind::CorporateAction);
        assert_eq!(spin.isin.as_deref(), Some("DE000ENER6Y0"));
        assert_eq!((spin.account, spin.quantity), (Account::Pea, Some(d("2"))));
    }

    #[test]
    fn bonus_shares_use_the_event_isin() {
        let e = corporate_action(
            "bonus",
            "2026-07-13T10:00:00.000+0000",
            "L'Oréal",
            "logos/FR0000120321/v2",
            "L'Oréal",
            "0.300000",
        );
        let p = &postings_for(&e, &accounts()).unwrap()[0];
        assert_eq!(
            (p.isin.as_deref(), p.quantity),
            (Some("FR0000120321"), Some(d("0.3")))
        );
    }

    #[test]
    fn meeting_invitations_produce_nothing() {
        let e = event(
            "SSP_CORPORATE_ACTION_ACTIVITY",
            json!({"subtitle": "Jährliche Hauptversammlung", "details": {"sections": []}}),
        );
        assert!(postings_for(&e, &accounts()).unwrap().is_empty());
    }

    #[test]
    fn rejects_unknown_event_types() {
        let e = event("SAVINGS_PLAN_SOMETHING_NEW", json!({}));
        assert!(matches!(
            postings_for(&e, &accounts()),
            Err(Error::UnknownEvent { .. })
        ));
    }

    #[test]
    fn detects_pea_account_from_trades() {
        let trade = event(
            "TRADING_TRADE_EXECUTED",
            json!({"cashAccountNumber": "B", "details": {"sections": [table("Übersicht", &[("Portfolio", "PEA")])]}}),
        );
        let other = event("INTEREST_PAYOUT", json!({"cashAccountNumber": "A"}));
        let map = AccountMap::detect([&trade, &other], &AccountMap::default()).unwrap();
        assert_eq!(map.pea_cash_account.as_deref(), Some("B"));
        assert_eq!(map.regular_cash_account.as_deref(), Some("A"));
    }
}
